//! Minimal blocking JSON-RPC client shared by the `fetch_block`, `fetch_continuous` and
//! `replay_7702` binaries. Not a binary target itself (see `autobins = false` in Cargo.toml).

#![allow(dead_code, unreachable_pub)] // shared module: each binary uses a subset

use std::{collections::BTreeMap, time::Duration};

use grevm::test_utils::common::mainnet::{self, AccountFixture, PreState, TxFixture};
use revm_primitives::{Address, B256, Bytes, U256};
use serde_json::{Value, json};

// `Send + Sync` so errors can cross a thread boundary (the `replay_7702` prefetch thread).
type Error = Box<dyn std::error::Error + Send + Sync>;

const RETRY_ATTEMPTS: usize = 7;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(400);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);
const BLOCK_HASH_BATCH_SIZE: usize = 32;
const ACCOUNT_CODE_BATCH_SIZE: usize = 64;

fn backoff(delay: &mut Duration) {
    std::thread::sleep(*delay);
    *delay = (*delay * 2).min(MAX_RETRY_DELAY);
}

fn is_rate_limit_error(error: &Value) -> bool {
    let code = error.get("code").and_then(Value::as_i64);
    let message =
        error.get("message").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    matches!(code, Some(429 | -32005 | -32007)) ||
        message.contains("rate limit") ||
        message.contains("request limit") ||
        message.contains("too many requests")
}

fn response_id(response: &Value) -> Option<u64> {
    let id = response.get("id")?;
    id.as_u64().or_else(|| id.as_str()?.parse().ok())
}

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
        let mut delay = INITIAL_RETRY_DELAY;
        for attempt in 0..RETRY_ATTEMPTS {
            match self.agent.post(&self.url).send_json(body) {
                Ok(resp) => return resp.into_json().map_err(Into::into),
                Err(ureq::Error::Status(code, _)) if code == 429 || code >= 500 => {
                    if attempt + 1 == RETRY_ATTEMPTS {
                        return Err(format!("HTTP {code} after {} retries", attempt + 1).into());
                    }
                    backoff(&mut delay);
                }
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
    }

    /// Issue a JSON-RPC call and return its `result` (or `Null`).
    ///
    /// Some providers report rate limits as a JSON-RPC error inside an HTTP 200 response. Retry
    /// those errors here; transport-level throttling remains the responsibility of [`Self::send`].
    pub fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut delay = INITIAL_RETRY_DELAY;
        for attempt in 0..RETRY_ATTEMPTS {
            let resp = self.send(&body)?;
            if let Some(error) = resp.get("error") {
                if is_rate_limit_error(error) && attempt + 1 < RETRY_ATTEMPTS {
                    backoff(&mut delay);
                    continue;
                }
                return Err(format!("{method} failed: {error}").into());
            }
            return Ok(resp.get("result").cloned().unwrap_or(Value::Null))
        }
        unreachable!()
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
    /// Uses JSON-RPC batches, retries partial and rate-limited responses with exponential backoff,
    /// then fetches any residual entries individually. The requested range is always complete on
    /// success.
    pub fn fetch_block_hashes_into(
        &self,
        cache: &mut BTreeMap<u64, B256>,
        lo: u64,
        hi: u64,
    ) -> Result<(), Error> {
        let mut delay = INITIAL_RETRY_DELAY;
        for attempt in 0..RETRY_ATTEMPTS {
            let missing: Vec<u64> = (lo..=hi).filter(|n| !cache.contains_key(n)).collect();
            if missing.is_empty() {
                return Ok(());
            }
            if attempt > 0 {
                backoff(&mut delay);
            }
            let mut rate_limited = false;
            for chunk in missing.chunks(BLOCK_HASH_BATCH_SIZE) {
                let batch: Vec<Value> = chunk
                    .iter()
                    .map(|&b| {
                        json!({"jsonrpc":"2.0","id":b,"method":"eth_getBlockByNumber",
                               "params":[format!("0x{b:x}"), false]})
                    })
                    .collect();
                let resp = self.send(&Value::Array(batch))?;
                let Some(responses) = resp.as_array() else {
                    rate_limited = resp.get("error").is_some_and(is_rate_limit_error);
                    if rate_limited {
                        break;
                    }
                    continue
                };
                for item in responses {
                    if item.get("error").is_some_and(is_rate_limit_error) {
                        rate_limited = true;
                        continue
                    }
                    let id = response_id(item);
                    let hash =
                        item.get("result").and_then(|r| r.get("hash")).and_then(Value::as_str);
                    if let (Some(id), Some(hash)) = (id, hash) &&
                        chunk.contains(&id)
                    {
                        let hash = hash
                            .parse()
                            .map_err(|error| format!("invalid hash for block {id}: {error:?}"))?;
                        cache.insert(id, hash);
                    }
                }
                if rate_limited {
                    // Stop consuming provider capacity once any item reports throttling. The next
                    // pass retries only unresolved entries after a bounded backoff.
                    break
                }
            }
        }

        // Batch APIs may omit entries or apply stricter limits than individual requests. Resolve
        // the usually small remainder through `call`, which also handles JSON-RPC rate limiting.
        let missing: Vec<u64> = (lo..=hi).filter(|n| !cache.contains_key(n)).collect();
        for number in missing {
            let block = self
                .call("eth_getBlockByNumber", json!([format!("0x{number:x}"), false]))
                .map_err(|error| format!("could not fetch block {number} hash: {error}"))?;
            let hash = block
                .get("hash")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("block {number} returned no hash"))?
                .parse()
                .map_err(|error| format!("invalid hash for block {number}: {error:?}"))?;
            cache.insert(number, hash);
        }
        Ok(())
    }

    /// Convenience: fetch block hashes for `[lo, hi]` into a fresh map (one-shot, no reuse).
    pub fn fetch_block_hashes(&self, lo: u64, hi: u64) -> Result<BTreeMap<u64, B256>, Error> {
        let mut out = BTreeMap::new();
        self.fetch_block_hashes_into(&mut out, lo, hi)?;
        Ok(out)
    }

    /// Fetch account code at one historical block, using JSON-RPC batches and falling back to
    /// individual retrying calls for any response omitted or rejected by the batch endpoint.
    pub fn fetch_account_codes(
        &self,
        addresses: &[Address],
        at: &str,
    ) -> Result<BTreeMap<Address, Bytes>, Error> {
        let mut codes = BTreeMap::new();
        for (chunk_index, chunk) in addresses.chunks(ACCOUNT_CODE_BATCH_SIZE).enumerate() {
            let id_base = chunk_index * ACCOUNT_CODE_BATCH_SIZE;
            let batch: Vec<Value> = chunk
                .iter()
                .enumerate()
                .map(|(index, address)| {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id_base + index,
                        "method": "eth_getCode",
                        "params": [address.to_string(), at]
                    })
                })
                .collect();
            let response = self.send(&Value::Array(batch))?;
            let Some(items) = response.as_array() else {
                continue;
            };
            for item in items {
                if item.get("error").is_some() {
                    continue;
                }
                let Some(id) = response_id(item).map(|id| id as usize) else {
                    continue;
                };
                let Some(address) = addresses.get(id) else {
                    continue;
                };
                let Some(code) = item.get("result").and_then(Value::as_str) else {
                    continue;
                };
                let code: Bytes = code
                    .parse()
                    .map_err(|error| format!("invalid code for account {address}: {error:?}"))?;
                codes.insert(*address, code);
            }
        }

        for address in addresses {
            if codes.contains_key(address) {
                continue;
            }
            let code = self.call("eth_getCode", json!([address.to_string(), at]))?;
            let code = code
                .as_str()
                .ok_or_else(|| format!("eth_getCode returned no code for account {address}"))?;
            let code: Bytes = code
                .parse()
                .map_err(|error| format!("invalid code for account {address}: {error:?}"))?;
            codes.insert(*address, code);
        }
        Ok(codes)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_http_and_json_rpc_rate_limits() {
        assert!(is_rate_limit_error(&json!({"code": 429, "message": "throttled"})));
        assert!(is_rate_limit_error(
            &json!({"code": -32007, "message": "50/second request limit reached"})
        ));
        assert!(is_rate_limit_error(&json!({"code": -1, "message": "Too many requests"})));
        assert!(!is_rate_limit_error(&json!({"code": -32602, "message": "invalid params"})));
    }

    #[test]
    fn accepts_numeric_and_decimal_string_response_ids() {
        assert_eq!(response_id(&json!({"id": 42})), Some(42));
        assert_eq!(response_id(&json!({"id": "42"})), Some(42));
        assert_eq!(response_id(&json!({"id": "0x2a"})), None);
    }
}
