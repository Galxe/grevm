//! Discover mainnet blocks over JSON-RPC and replay each through grevm's parallel-vs-sequential
//! check — all in a single process.
//!
//! Blocks are scanned **upward** from `start_block` toward the chain head, keeping only those that
//! match a `filter`. A background thread discovers and fetches the *next* matching block while the
//! main thread replays the *current* one (`sync_channel(1)` keeps the fetcher exactly one block
//! ahead), so network I/O overlaps with execution without spawning a process per block.
//!
//! Discovery scans the chain directly via the RPC (no etherscan scraping / API key needed); a
//! `debug`-namespace endpoint with the built-in `prestateTracer` and `callTracer` is required.
//!
//! Usage:
//! ```text
//! cargo run --bin replay_mainnet --features tools -- <rpc_url> [filter] [start_block] [count] [out_dir]
//! ```
//! - `filter`      — which blocks to replay: `all` (default) every non-empty block, `eip-7702` only
//!   blocks containing a type-4 transaction, or `delegated-safety` blocks where delegated execution
//!   attempts CREATE/CREATE2 or a balance-moving operation.
//! - `start_block` — block to start scanning upward from. Default: the mainnet EIP-7702 activation
//!   block ([`PECTRA_BLOCK`]).
//! - `count`       — how many matching blocks to replay. Default: all of them up to the chain head.
//! - `out_dir`     — optional. If given, each replayed block's fixture is written to
//!   `<out_dir>/<number>/` (same format as `fetch_block`). Omitted ⇒ fetched in memory only.
//!
//! Each block is validated as it arrives and its parallel/sequential **execution-only** times
//! (no I/O) are accumulated; a final line reports the aggregate speedup. A sequential reference
//! error, Grevm error/skipped transaction, or result divergence fails immediately with a non-zero
//! exit.

mod rpc;

use std::{
    collections::{BTreeMap, BTreeSet},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use grevm::test_utils::common::{
    execute,
    mainnet::{
        self, AccountFixture, BlockFixture, MainnetBlock, PreState, TxFixture,
        spec_for_mainnet_block,
    },
};
use revm_context::transaction::AuthorizationTr;
use revm_primitives::{Address, B256};
use rpc::{Rpc, parse_block_number};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Which blocks to replay. New variants can be added without touching the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    /// Every block with at least one transaction.
    All,
    /// Only blocks containing an EIP-7702 (type-4) transaction.
    Eip7702,
    /// Candidate blocks exercising delegated CREATE/CREATE2 or delegated balance movement.
    ///
    /// The node's built-in `callTracer` finds relevant internal frames, then their source accounts
    /// are checked for an EIP-7702 delegation designator. Reverted frames remain candidates; the
    /// subsequent full replay resolves their final effect.
    DelegatedSafety,
}

impl Filter {
    fn parse(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "all" => Ok(Filter::All),
            "eip-7702" | "eip7702" | "7702" => Ok(Filter::Eip7702),
            "delegated-safety" => Ok(Filter::DelegatedSafety),
            other => Err(format!(
                "unknown filter {other:?} (expected `all`, `eip-7702`, or `delegated-safety`)"
            )
            .into()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Eip7702 => "EIP-7702",
            Filter::DelegatedSafety => "delegated safety",
        }
    }

    /// Whether a block (an `eth_getBlockByNumber` result with full txs) should be replayed.
    fn matches(self, rpc: &Rpc, block_number: u64, block: &Value) -> Result<bool, Error> {
        let txs = block.get("transactions").and_then(Value::as_array);
        match self {
            Filter::All => Ok(txs.is_some_and(|t| !t.is_empty())),
            Filter::Eip7702 => Ok(txs.is_some_and(|t| {
                t.iter().any(|x| x.get("type").and_then(Value::as_str) == Some("0x4"))
            })),
            Filter::DelegatedSafety => {
                if txs.is_none_or(Vec::is_empty) {
                    return Ok(false);
                }
                let block_tag = format!("0x{block_number:x}");
                let trace = rpc.call(
                    "debug_traceBlockByNumber",
                    json!([
                        block_tag,
                        {
                            "tracer": "callTracer",
                            "tracerConfig": { "onlyTopCall": false },
                            "timeout": "60s"
                        }
                    ]),
                )?;
                delegated_safety_trace_matches(rpc, block_number, block, &trace)
            }
        }
    }
}

/// One value-moving or contract-creation internal frame and its state-context source account.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DelegatedSafetyCandidate {
    tx_index: usize,
    source: Address,
}

/// Decide whether a built-in `callTracer` result contains a delegated-safety candidate.
///
/// For CALL/CREATE/SELFDESTRUCT frames, `from` is the actual state-context account whose balance or
/// nonce is affected. This stays the EIP-7702 authority even when its delegated code reaches the
/// operation through DELEGATECALL. An authority is conservatively considered delegated when:
/// - its parent-block code is an EIP-7702 designator; or
/// - a recoverable, non-clearing authorization for it appears no later than the candidate tx.
///
/// Authorization nonce/chain validation and reverts are intentionally left to the full replay.
/// That can add candidates but cannot hide an applicable delegated-safety operation.
fn delegated_safety_trace_matches(
    rpc: &Rpc,
    block_number: u64,
    block: &Value,
    trace: &Value,
) -> Result<bool, Error> {
    let tx_values = block
        .get("transactions")
        .and_then(Value::as_array)
        .ok_or("full block response did not contain a transaction array")?;
    let candidates = delegated_safety_candidates(trace, tx_values.len())?;
    if candidates.is_empty() {
        return Ok(false);
    }

    let txs: Vec<TxFixture> =
        tx_values.iter().map(TxFixture::from_rpc).collect::<Result<_, _>>()?;
    let authorizations = earliest_non_clearing_authorizations(&txs);
    if candidates.iter().any(|candidate| {
        authorizations
            .get(&candidate.source)
            .is_some_and(|&tx_index| tx_index <= candidate.tx_index)
    }) {
        return Ok(true);
    }

    let sources: Vec<Address> = candidates
        .iter()
        .map(|candidate| candidate.source)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let parent = format!("0x{:x}", block_number.saturating_sub(1));
    let codes = rpc.fetch_account_codes(&sources, &parent)?;
    Ok(codes.values().any(|code| is_eip7702_designator(code)))
}

/// Extract relevant internal frames from a block-level built-in `callTracer` response.
fn delegated_safety_candidates(
    trace: &Value,
    expected_transactions: usize,
) -> Result<BTreeSet<DelegatedSafetyCandidate>, String> {
    let transactions = trace
        .as_array()
        .ok_or_else(|| format!("callTracer returned a non-array result: {trace}"))?;
    if transactions.len() != expected_transactions {
        return Err(format!(
            "callTracer returned {} transaction results for a block with {expected_transactions} \
             transactions",
            transactions.len()
        ));
    }

    let mut candidates = BTreeSet::new();
    for (tx_index, transaction) in transactions.iter().enumerate() {
        if let Some(error) = transaction.get("error").filter(|error| !error.is_null()) {
            return Err(format!("callTracer failed for transaction {tx_index}: {error}"));
        }
        let root = transaction.get("result").unwrap_or(transaction);
        collect_delegated_safety_frames(root, tx_index, true, &mut candidates)?;
    }
    Ok(candidates)
}

fn collect_delegated_safety_frames(
    frame: &Value,
    tx_index: usize,
    is_root: bool,
    candidates: &mut BTreeSet<DelegatedSafetyCandidate>,
) -> Result<(), String> {
    let frame = frame
        .as_object()
        .ok_or_else(|| format!("callTracer transaction {tx_index} contained a non-object frame"))?;
    let frame_type = frame.get("type").and_then(Value::as_str).unwrap_or_default();
    let is_candidate = if is_root {
        false
    } else {
        match frame_type {
            "CREATE" | "CREATE2" | "SELFDESTRUCT" => true,
            "CALL" => {
                let value = frame.get("value").and_then(Value::as_str).unwrap_or("0x0");
                let value = value.parse::<revm_primitives::U256>().map_err(|error| {
                    format!("callTracer transaction {tx_index} returned invalid value: {error:?}")
                })?;
                let from = frame.get("from").and_then(Value::as_str);
                let to = frame.get("to").and_then(Value::as_str);
                let different_accounts = match (from, to) {
                    (Some(from), Some(to)) => !from.eq_ignore_ascii_case(to),
                    _ => true,
                };
                !value.is_zero() && different_accounts
            }
            _ => false,
        }
    };
    if is_candidate {
        let source = frame
            .get("from")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!("callTracer transaction {tx_index} {frame_type} frame omitted `from`")
            })?
            .parse::<Address>()
            .map_err(|error| {
                format!(
                    "callTracer transaction {tx_index} {frame_type} frame has invalid `from`: \
                     {error:?}"
                )
            })?;
        candidates.insert(DelegatedSafetyCandidate { tx_index, source });
    }
    if let Some(children) = frame.get("calls") {
        let children = children.as_array().ok_or_else(|| {
            format!("callTracer transaction {tx_index} returned a non-array `calls` field")
        })?;
        for child in children {
            collect_delegated_safety_frames(child, tx_index, false, candidates)?;
        }
    }
    Ok(())
}

fn earliest_non_clearing_authorizations(txs: &[TxFixture]) -> BTreeMap<Address, usize> {
    let mut earliest = BTreeMap::new();
    for (tx_index, tx) in txs.iter().enumerate() {
        for authorization in tx.to_tx_env().authorization_list {
            if authorization.address() != Address::ZERO &&
                let Some(authority) = authorization.authority()
            {
                earliest.entry(authority).or_insert(tx_index);
            }
        }
    }
    earliest
}

fn is_eip7702_designator(code: &[u8]) -> bool {
    code.len() == 23 && code.starts_with(&[0xef, 0x01, 0x00])
}

/// Per-run caches reused across the (sequentially scanned) blocks to avoid re-fetching state that
/// barely changes between adjacent blocks.
#[derive(Default)]
struct Caches {
    /// Block number -> hash, for the `BLOCKHASH` opcode (256-block windows overlap across blocks).
    hashes: BTreeMap<u64, B256>,
    /// EIP-7702 delegation target -> account (or `None` if not a contract); targets recur.
    delegates: BTreeMap<Address, Option<AccountFixture>>,
}

/// Mainnet block at which EIP-7702 (the Pectra hardfork) activated, 2025-05-07 — the default start
/// block. Hardcoded for Ethereum mainnet; pass an explicit `start_block` for other chains.
const PECTRA_BLOCK: u64 = 22_431_084;

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: cargo run --bin replay_mainnet --features tools -- \
             <rpc_url> [all|eip-7702|delegated-safety] [start_block] [count] [out_dir]"
        );
        std::process::exit(1);
    }
    let rpc_url = args[1].clone();
    let filter = match args.get(2) {
        Some(s) => Filter::parse(s)?,
        None => Filter::All,
    };
    let start_arg = args.get(3).cloned();
    let count: Option<usize> = args.get(4).map(|s| s.parse()).transpose()?;
    let save_dir: Option<PathBuf> = args.get(5).map(PathBuf::from);

    // Force the parallel path even for small blocks, and print grevm's per-block metrics. SAFETY:
    // set before any thread is spawned and before any execution reads them — no concurrent
    // getenv/setenv.
    if std::env::var_os("GREVM_MIN_PARALLEL_TXS").is_none() {
        unsafe { std::env::set_var("GREVM_MIN_PARALLEL_TXS", "0") };
    }
    if std::env::var_os("GREVM_PRINT_METRICS").is_none() {
        unsafe { std::env::set_var("GREVM_PRINT_METRICS", "1") };
    }

    let rpc = Rpc::new(rpc_url.clone());
    let head = rpc.head_block()?;
    let start = match start_arg {
        Some(s) => parse_block_number(&s)?,
        None => PECTRA_BLOCK,
    };
    let what = filter.label();
    match count {
        Some(c) => {
            println!("Replaying up to {c} block(s) [filter: {what}] from {start} (head {head})")
        }
        None => println!("Replaying every block [filter: {what}] from {start} up to head {head}"),
    }
    if let Some(dir) = &save_dir {
        println!("Persisting each block's fixture under {}", dir.display());
    }

    // Prefetch thread: discover + fetch the next matching block while the main thread replays the
    // current one. `sync_channel(1)` keeps it exactly one block ahead.
    let (tx, rx) = mpsc::sync_channel::<MainnetBlock>(1);
    let fetcher = thread::spawn(move || -> Result<(), Error> {
        let rpc = Rpc::new(rpc_url);
        let chain_id = rpc.chain_id()?;
        // Reused across blocks to keep per-block RPC volume low (block hashes + delegate targets).
        let mut caches = Caches::default();
        let mut from = start;
        let mut produced = 0usize;
        loop {
            if count.is_some_and(|c| produced >= c) {
                break;
            }
            match next_match(&rpc, filter, from, head, chain_id, save_dir.as_deref(), &mut caches)?
            {
                Some(block) => {
                    let next = block.number + 1;
                    if tx.send(block).is_err() {
                        break; // receiver gone
                    }
                    from = next;
                    produced += 1;
                }
                None => break, // reached head, no more matching blocks
            }
        }
        Ok(())
    });

    let mut replayed = 0usize;
    let (mut total_seq, mut total_par) = (Duration::ZERO, Duration::ZERO);
    for block in rx {
        let (label, ntx, spec) = (block.label.clone(), block.txs.len(), block.spec);
        println!("===== replay block {label} ({ntx} txs, spec {spec:?}) =====");
        // The sequential reference runs first inside the comparison. Mainnet contains no invalid
        // transactions: a reference error means the fixture/environment is wrong, while a Grevm
        // error, skipped transaction, or divergence is a Grevm failure. Every case panics and is
        // converted here into a fail-fast non-zero exit.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            execute::compare_evm_execute_with_env(
                block.db,
                block.txs,
                block.cfg,
                block.block_env,
                Default::default(),
            )
        }));
        match outcome {
            Ok(execute::ReplayTimings { sequential, parallel }) => {
                replayed += 1;
                total_seq += sequential;
                total_par += parallel;
                println!(
                    "  block {label}: OK (execution only: sequential {sequential:?}, \
                     parallel {parallel:?})"
                );
            }
            Err(_) => {
                eprintln!(
                    "\nFAILED at block {label}: invalid mainnet replay input, Grevm execution \
                     error/skipped transaction, or parallel result divergence \
                     (see the panic above for details)"
                );
                std::process::exit(1);
            }
        }
    }

    fetcher.join().map_err(|_| "prefetch thread panicked")??;

    println!("Done: {replayed} blocks passed");
    if replayed > 0 && total_par > Duration::ZERO {
        let speedup = total_seq.as_secs_f64() / total_par.as_secs_f64();
        println!(
            "Aggregate execution time (no I/O) over {replayed} blocks: sequential {total_seq:?}, \
             parallel {total_par:?}  →  {speedup:.2}x"
        );
    }
    Ok(())
}

/// Scan upward from `from` to `head` (inclusive), returning the first block (built in memory) that
/// satisfies `filter`, or `None` if none remain. If `save_dir` is set, the fixture is also written.
fn next_match(
    rpc: &Rpc,
    filter: Filter,
    from: u64,
    head: u64,
    chain_id: u64,
    save_dir: Option<&Path>,
    caches: &mut Caches,
) -> Result<Option<MainnetBlock>, Error> {
    let mut bn = from;
    while bn <= head {
        let hex = format!("0x{bn:x}");
        let block = rpc.call("eth_getBlockByNumber", json!([hex, true]))?;
        if !block.is_null() && filter.matches(rpc, bn, &block)? {
            return Ok(Some(build_block(rpc, bn, chain_id, &block, save_dir, caches)?));
        }
        bn += 1;
    }
    Ok(None)
}

/// Build a [`MainnetBlock`] in memory from an already-fetched `eth_getBlockByNumber` result plus a
/// fresh `prestateTracer` trace.
fn build_block(
    rpc: &Rpc,
    number: u64,
    chain_id: u64,
    block: &Value,
    save_dir: Option<&Path>,
    caches: &mut Caches,
) -> Result<MainnetBlock, Error> {
    let ts = block.get("timestamp").and_then(Value::as_str).unwrap_or("0x0");
    let spec = spec_for_mainnet_block(
        number,
        u64::from_str_radix(ts.trim_start_matches("0x"), 16).unwrap_or(0),
    )
    .to_string();
    let mut bf = BlockFixture::from_rpc(number, chain_id, spec, block)?;
    // Block hashes for BLOCKHASH, reusing the cross-block cache (fetches only the few new entries).
    let (lo, hi) = (number.saturating_sub(256), number.saturating_sub(1));
    rpc.fetch_block_hashes_into(&mut caches.hashes, lo, hi)?;
    bf.block_hashes = caches.hashes.range(lo..=hi).map(|(k, v)| (*k, *v)).collect();
    caches.hashes.retain(|&k, _| k >= lo); // bound memory to the sliding window
    let txs: Vec<TxFixture> = block
        .get("transactions")
        .and_then(Value::as_array)
        .map(|v| v.iter().map(TxFixture::from_rpc).collect::<Result<_, _>>())
        .transpose()?
        .unwrap_or_default();
    let trace = rpc.call(
        "debug_traceBlockByNumber",
        json!([format!("0x{number:x}"), { "tracer": "prestateTracer" }]),
    )?;
    let mut pre_state = PreState::new();
    mainnet::accumulate_prestate(&mut pre_state, &trace, &txs)?;
    // prestateTracer omits EIP-7702 delegation targets' code; fetch them (cached across blocks).
    let parent = format!("0x{:x}", number.saturating_sub(1));
    rpc.supplement_delegations_cached(&txs, &mut pre_state, &parent, &mut caches.delegates)?;
    if let Some(dir) = save_dir {
        mainnet::write_mainnet_block(dir, &bf, &txs, &pre_state)?;
    }
    Ok(MainnetBlock::from_fixtures(number.to_string(), &bf, &txs, &pre_state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delegated_safety_filter() {
        assert_eq!(Filter::parse("delegated-safety").unwrap(), Filter::DelegatedSafety);
        assert!(Filter::parse("delegated_safety").is_err());
    }

    #[test]
    fn extracts_delegated_safety_sources_from_internal_frames() {
        let authority = "0x1111111111111111111111111111111111111111";
        let creator = "0x2222222222222222222222222222222222222222";
        let selfdestruct = "0x3333333333333333333333333333333333333333";
        let target = "0x4444444444444444444444444444444444444444";
        let trace = json!([
            {
                "txHash": "0x01",
                "result": {
                    "type": "CALL",
                    "from": authority,
                    "to": target,
                    "value": "0x10",
                    "calls": [
                        {
                            "type": "CALL",
                            "from": authority,
                            "to": target,
                            "value": "0x1"
                        },
                        {
                            "type": "CALL",
                            "from": authority,
                            "to": target,
                            "value": "0x0"
                        },
                        {
                            "type": "CALL",
                            "from": authority,
                            "to": authority.to_ascii_uppercase(),
                            "value": "0x1"
                        },
                        {
                            "type": "CREATE2",
                            "from": creator,
                            "to": target,
                            "value": "0x0"
                        },
                        {
                            "type": "DELEGATECALL",
                            "from": authority,
                            "to": target,
                            "calls": [{
                                "type": "CALL",
                                "from": authority,
                                "to": target,
                                "value": "0x2"
                            }]
                        }
                    ]
                }
            },
            {
                "txHash": "0x02",
                "result": {
                    "type": "CALL",
                    "from": target,
                    "to": authority,
                    "calls": [{
                        "type": "SELFDESTRUCT",
                        "from": selfdestruct,
                        "to": target,
                        "value": "0x0"
                    }]
                }
            }
        ]);
        let candidates = delegated_safety_candidates(&trace, 2).unwrap();
        assert_eq!(
            candidates,
            BTreeSet::from([
                DelegatedSafetyCandidate { tx_index: 0, source: authority.parse().unwrap() },
                DelegatedSafetyCandidate { tx_index: 0, source: creator.parse().unwrap() },
                DelegatedSafetyCandidate { tx_index: 1, source: selfdestruct.parse().unwrap() },
            ])
        );
    }

    #[test]
    fn ignores_roots_and_irrelevant_call_types() {
        let trace = json!([{
            "result": {
                "type": "CREATE",
                "from": "0x1111111111111111111111111111111111111111",
                "value": "0x1",
                "calls": [
                    {
                        "type": "STATICCALL",
                        "from": "0x1111111111111111111111111111111111111111",
                        "to": "0x2222222222222222222222222222222222222222"
                    },
                    {
                        "type": "CALLCODE",
                        "from": "0x1111111111111111111111111111111111111111",
                        "to": "0x2222222222222222222222222222222222222222",
                        "value": "0x1"
                    }
                ]
            }
        }]);
        assert!(delegated_safety_candidates(&trace, 1).unwrap().is_empty());
    }

    #[test]
    fn reports_malformed_and_per_transaction_call_tracer_errors() {
        assert!(delegated_safety_candidates(&Value::Null, 0).is_err());
        assert!(delegated_safety_candidates(&json!([]), 1).is_err());
        assert!(
            delegated_safety_candidates(
                &json!([
                { "txHash": "0x01", "error": "tracer timed out" }
                ]),
                1
            )
            .is_err()
        );
        assert!(
            delegated_safety_candidates(
                &json!([
                    { "txHash": "0x01", "result": { "type": "CALL", "calls": {} } }
                ]),
                1
            )
            .is_err()
        );
    }

    #[test]
    fn recognizes_only_complete_eip7702_designators() {
        let mut designator = vec![0xef, 0x01, 0x00];
        designator.extend([0x11; 20]);
        assert!(is_eip7702_designator(&designator));
        assert!(!is_eip7702_designator(&designator[..22]));
        designator.push(0);
        assert!(!is_eip7702_designator(&designator));
    }

    #[test]
    fn recovers_same_transaction_mainnet_delegation_authority() {
        // Mainnet block 23_352_852, tx index 26. The authorization delegates this authority and
        // its callTracer frame then executes CREATE2 from the same state context.
        let tx = TxFixture {
            tx_type: 4,
            caller: Address::ZERO,
            to: Some(Address::ZERO),
            nonce: 0,
            value: revm_primitives::U256::ZERO,
            data: Default::default(),
            gas_limit: 0,
            gas_price: 0,
            gas_priority_fee: None,
            chain_id: Some(1),
            access_list: Vec::new(),
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            authorization_list: vec![mainnet::AuthFixture {
                chain_id: revm_primitives::U256::from(1),
                address: "0x80296ff8d1ed46f8e3c7992664d13b833504c2bb".parse().unwrap(),
                nonce: 0x58,
                y_parity: 1,
                r: "0x929f6cd0d7b45d3327876760190d01dc949ae1ad5d0118287c9fa303327199a2"
                    .parse()
                    .unwrap(),
                s: "0x1ec1a55dc4d3312c58d3d6590eca1c03032763a5c5f438548df8d49d1e725bcd"
                    .parse()
                    .unwrap(),
            }],
        };
        assert_eq!(
            earliest_non_clearing_authorizations(&[tx]),
            BTreeMap::from([("0x8fcf555a7a664654af595a72e4e48676db8136cf".parse().unwrap(), 0,)])
        );
    }
}
