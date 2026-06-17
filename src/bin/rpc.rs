//! Minimal blocking JSON-RPC client shared by the `fetch_block`, `fetch_continuous` and
//! `replay_7702` binaries. Not a binary target itself (see `autobins = false` in Cargo.toml).

#![allow(dead_code, unreachable_pub)] // shared module: each binary uses a subset

use std::{collections::BTreeMap, time::Duration};

use grevm::test_utils::common::mainnet::{self, AccountFixture, PreState, TxFixture};
use revm_primitives::{Address, B256, Bytes, U256};
use serde_json::{Value, json};

// `Send + Sync` so errors can cross a thread boundary (the `replay_7702` prefetch thread).
type Error = Box<dyn std::error::Error + Send + Sync>;

/// A tiny JSON-RPC-over-HTTP client.
pub struct Rpc {
    agent: ureq::Agent,
    url: String,
}

impl Rpc {
    pub fn new(url: String) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(180)).build();
        Self { agent, url }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// POST a JSON body, retrying on HTTP 429 (Too Many Requests) and 5xx with exponential backoff.
    fn send(&self, body: &Value) -> Result<Value, Error> {
        let mut delay = Duration::from_millis(400);
        for attempt in 0..7 {
            match self.agent.post(&self.url).send_json(body) {
                Ok(resp) => return resp.into_json().map_err(Into::into),
                Err(ureq::Error::Status(code, _)) if code == 429 || code >= 500 => {
                    if attempt == 6 {
                        return Err(format!("HTTP {code} after {} retries", attempt + 1).into());
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(10));
                }
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
    }

    /// Issue a JSON-RPC call and return its `result` (or `Null`).
    pub fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self.send(&body)?;
        if let Some(err) = resp.get("error") {
            return Err(format!("{method} failed: {err}").into());
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// `eth_chainId`, defaulting to mainnet (1) if unavailable.
    pub fn chain_id(&self) -> Result<u64, Error> {
        let v = self.call("eth_chainId", json!([]))?;
        let s = v.as_str().unwrap_or("0x1");
        Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16).unwrap_or(1))
    }

    /// `eth_blockNumber` — current chain head.
    pub fn head_block(&self) -> Result<u64, Error> {
        let v = self.call("eth_blockNumber", json!([]))?;
        let s = v.as_str().ok_or("eth_blockNumber returned no result")?;
        Ok(u64::from_str_radix(s.trim_start_matches("0x"), 16)?)
    }

    /// Fetch block hashes for `[lo, hi]` (number -> hash) into `cache`, fetching only entries not
    /// already present. Callers replaying consecutive blocks can reuse one `cache` across blocks
    /// (the 256-hash windows overlap almost entirely), turning ~256 fetches/block into ~1.
    ///
    /// Uses JSON-RPC batches of 32 (with 429/5xx backoff) and retries entries a batch dropped, so
    /// the requested range ends up complete; errors otherwise.
    pub fn fetch_block_hashes_into(
        &self,
        cache: &mut BTreeMap<u64, B256>,
        lo: u64,
        hi: u64,
    ) -> Result<(), Error> {
        for attempt in 0..5 {
            let missing: Vec<u64> = (lo..=hi).filter(|n| !cache.contains_key(n)).collect();
            if missing.is_empty() {
                return Ok(());
            }
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(250));
            }
            for chunk in missing.chunks(32) {
                let batch: Vec<Value> = chunk
                    .iter()
                    .map(|&b| {
                        json!({"jsonrpc":"2.0","id":b,"method":"eth_getBlockByNumber",
                               "params":[format!("0x{b:x}"), false]})
                    })
                    .collect();
                let resp = self.send(&Value::Array(batch))?;
                let Some(arr) = resp.as_array() else { continue };
                for item in arr {
                    let id = item.get("id").and_then(Value::as_u64);
                    let hash =
                        item.get("result").and_then(|r| r.get("hash")).and_then(Value::as_str);
                    if let (Some(id), Some(hash)) = (id, hash) {
                        cache.insert(id, hash.parse().map_err(|e| format!("{e:?}"))?);
                    }
                }
            }
        }
        let missing = (lo..=hi).filter(|n| !cache.contains_key(n)).count();
        if missing > 0 {
            return Err(format!("could not fetch {missing} block hashes in [{lo}, {hi}]").into());
        }
        Ok(())
    }

    /// Convenience: fetch block hashes for `[lo, hi]` into a fresh map (one-shot, no reuse).
    pub fn fetch_block_hashes(&self, lo: u64, hi: u64) -> Result<BTreeMap<u64, B256>, Error> {
        let mut out = BTreeMap::new();
        self.fetch_block_hashes_into(&mut out, lo, hi)?;
        Ok(out)
    }

    /// Add the EIP-7702 delegation **targets** of `txs`/`pre_state` to `pre_state` (at block `at`,
    /// e.g. the parent block hex), fetching each target's code/balance/nonce over RPC.
    ///
    /// `prestateTracer` omits these targets' code, so a call into a delegated account would
    /// otherwise replay as a call to an empty account. Returns the number of accounts added.
    pub fn supplement_delegations(
        &self,
        txs: &[TxFixture],
        pre_state: &mut PreState,
        at: &str,
    ) -> Result<usize, Error> {
        self.supplement_delegations_cached(txs, pre_state, at, &mut BTreeMap::new())
    }

    /// Like [`Self::supplement_delegations`] but reuses `cache` (target address -> fetched account,
    /// or `None` for "not a contract") across blocks. Delegation targets (e.g. the MetaMask/OKX
    /// delegators) recur in nearly every 7702 block, so caching avoids re-fetching them each time.
    pub fn supplement_delegations_cached(
        &self,
        txs: &[TxFixture],
        pre_state: &mut PreState,
        at: &str,
        cache: &mut BTreeMap<Address, Option<AccountFixture>>,
    ) -> Result<usize, Error> {
        let mut added = 0usize;
        for target in mainnet::delegation_targets(txs, pre_state) {
            // Skip if the prestate already carries its code.
            if pre_state
                .get(&target)
                .is_some_and(|a| a.code.as_ref().is_some_and(|c| !c.is_empty()))
            {
                continue;
            }
            let account = match cache.get(&target) {
                Some(cached) => cached.clone(),
                None => {
                    let fetched = self.fetch_account_with_code(&target.to_string(), at)?;
                    cache.insert(target, fetched.clone());
                    fetched
                }
            };
            if let Some(account) = account {
                pre_state.insert(target, account);
                added += 1;
            }
        }
        Ok(added)
    }

    /// Fetch an account's code/balance/nonce at block `at`. Returns `None` if it has no code (an
    /// EOA / cleared delegation), since only contract targets matter here.
    fn fetch_account_with_code(
        &self,
        addr: &str,
        at: &str,
    ) -> Result<Option<AccountFixture>, Error> {
        let code = self.call("eth_getCode", json!([addr, at]))?;
        let code = code.as_str().unwrap_or("0x");
        if code == "0x" || code.is_empty() {
            return Ok(None);
        }
        let balance = self.call("eth_getBalance", json!([addr, at]))?;
        let nonce = self.call("eth_getTransactionCount", json!([addr, at]))?;
        let balance: U256 =
            balance.as_str().unwrap_or("0x0").parse().map_err(|e| format!("{e:?}"))?;
        let nonce =
            u64::from_str_radix(nonce.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)
                .unwrap_or(0);
        let code: Bytes = code.parse().map_err(|e| format!("{e:?}"))?;
        Ok(Some(AccountFixture { balance, nonce, code: Some(code), storage: Default::default() }))
    }
}

/// Parse a CLI block argument as decimal (`25323281`) or hex (`0x1826711`).
pub fn parse_block_number(s: &str) -> Result<u64, Error> {
    let n = if let Some(hex) = s.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)?
    } else {
        s.parse::<u64>()?
    };
    Ok(n)
}
