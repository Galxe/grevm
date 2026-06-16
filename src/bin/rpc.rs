//! Minimal blocking JSON-RPC client shared by the `fetch_block`, `fetch_continuous` and
//! `replay_7702` binaries. Not a binary target itself (see `autobins = false` in Cargo.toml).

#![allow(dead_code, unreachable_pub)] // shared module: each binary uses a subset

use std::{collections::BTreeMap, time::Duration};

use grevm::test_utils::common::mainnet::{self, AccountFixture, PreState, TxFixture};
use revm_primitives::{B256, Bytes, U256};
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

    /// Issue a JSON-RPC call and return its `result` (or `Null`).
    pub fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self.agent.post(&self.url).send_json(body)?.into_json()?;
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

    /// Fetch block hashes for the inclusive range `[lo, hi]` (number -> hash), for the `BLOCKHASH`
    /// opcode. Uses JSON-RPC batches of 32 and retries any entries a (possibly rate-limited) batch
    /// dropped, so the result is complete. Errors if some hashes still can't be fetched.
    pub fn fetch_block_hashes(&self, lo: u64, hi: u64) -> Result<BTreeMap<u64, B256>, Error> {
        let mut out: BTreeMap<u64, B256> = BTreeMap::new();
        for attempt in 0..5 {
            let missing: Vec<u64> = (lo..=hi).filter(|n| !out.contains_key(n)).collect();
            if missing.is_empty() {
                break;
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
                let resp: Value =
                    self.agent.post(&self.url).send_json(Value::Array(batch))?.into_json()?;
                let Some(arr) = resp.as_array() else { continue };
                for item in arr {
                    let id = item.get("id").and_then(Value::as_u64);
                    let hash =
                        item.get("result").and_then(|r| r.get("hash")).and_then(Value::as_str);
                    if let (Some(id), Some(hash)) = (id, hash) {
                        out.insert(id, hash.parse().map_err(|e| format!("{e:?}"))?);
                    }
                }
            }
        }
        let missing = (lo..=hi).filter(|n| !out.contains_key(n)).count();
        if missing > 0 {
            return Err(format!("could not fetch {missing} block hashes in [{lo}, {hi}]").into());
        }
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
        let mut added = 0usize;
        for target in mainnet::delegation_targets(txs, pre_state) {
            // Skip if we already have its code.
            if pre_state
                .get(&target)
                .is_some_and(|a| a.code.as_ref().is_some_and(|c| !c.is_empty()))
            {
                continue;
            }
            let addr = target.to_string();
            let code = self.call("eth_getCode", json!([addr, at]))?;
            let code = code.as_str().unwrap_or("0x");
            if code == "0x" || code.is_empty() {
                continue; // not a contract (e.g. delegation already cleared)
            }
            let balance = self.call("eth_getBalance", json!([addr, at]))?;
            let nonce = self.call("eth_getTransactionCount", json!([addr, at]))?;
            let balance: U256 =
                balance.as_str().unwrap_or("0x0").parse().map_err(|e| format!("{e:?}"))?;
            let nonce =
                u64::from_str_radix(nonce.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)
                    .unwrap_or(0);
            let code: Bytes = code.parse().map_err(|e| format!("{e:?}"))?;
            pre_state.insert(
                target,
                AccountFixture { balance, nonce, code: Some(code), storage: Default::default() },
            );
            added += 1;
        }
        Ok(added)
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
