//! Discover mainnet blocks over JSON-RPC and replay each through grevm's parallel-vs-sequential
//! check — all in a single process.
//!
//! Blocks are scanned **upward** from `start_block` toward the chain head, keeping only those that
//! match a `filter`. A background thread discovers and fetches the *next* matching block while the
//! main thread replays the *current* one (`sync_channel(1)` keeps the fetcher exactly one block
//! ahead), so network I/O overlaps with execution without spawning a process per block.
//!
//! Discovery scans the chain directly via the RPC (no etherscan scraping / API key needed); a
//! `debug`-namespace endpoint (for `prestateTracer`) is required.
//!
//! Usage:
//! ```text
//! cargo run --bin replay_mainnet --features tools -- <rpc_url> [filter] [start_block] [count] [out_dir]
//! ```
//! - `filter`      — which blocks to replay: `all` (default) every non-empty block, or `eip-7702`
//!   only blocks containing a type-4 transaction. More filters can be added later.
//! - `start_block` — block to start scanning upward from. Default: the mainnet EIP-7702 activation
//!   block ([`PECTRA_BLOCK`]).
//! - `count`       — how many matching blocks to replay. Default: all of them up to the chain head.
//! - `out_dir`     — optional. If given, each replayed block's fixture is written to
//!   `<out_dir>/<number>/` (same format as `fetch_block`). Omitted ⇒ fetched in memory only.
//!
//! Each block is validated as it arrives and its parallel/sequential **execution-only** times
//! (no I/O) are accumulated; a final line reports the aggregate speedup. On the first divergence
//! (grevm parallel result != sequential) the offending block is reported and the process exits
//! non-zero.

mod rpc;

use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use grevm::test_utils::common::{
    execute,
    mainnet::{
        self, AccountFixture, BlockFixture, MainnetBlock, PreState, TxFixture, spec_for_timestamp,
    },
};
use revm_primitives::{Address, B256};
use rpc::{Rpc, parse_block_number};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Which blocks to replay. New variants can be added without touching the pipeline.
#[derive(Clone, Copy)]
enum Filter {
    /// Every block with at least one transaction.
    All,
    /// Only blocks containing an EIP-7702 (type-4) transaction.
    Eip7702,
}

impl Filter {
    fn parse(s: &str) -> Result<Self, Error> {
        match s.to_ascii_lowercase().as_str() {
            "all" => Ok(Filter::All),
            "eip-7702" | "eip7702" | "7702" => Ok(Filter::Eip7702),
            other => Err(format!("unknown filter {other:?} (expected `all` or `eip-7702`)").into()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Eip7702 => "EIP-7702",
        }
    }

    /// Whether a block (an `eth_getBlockByNumber` result with full txs) should be replayed.
    fn matches(self, block: &Value) -> bool {
        let txs = block.get("transactions").and_then(Value::as_array);
        match self {
            Filter::All => txs.is_some_and(|t| !t.is_empty()),
            Filter::Eip7702 => txs.is_some_and(|t| {
                t.iter().any(|x| x.get("type").and_then(Value::as_str) == Some("0x4"))
            }),
        }
    }
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
             <rpc_url> [all|eip-7702] [start_block] [count] [out_dir]"
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
    let mut skipped = 0usize;
    let (mut total_seq, mut total_par) = (Duration::ZERO, Duration::ZERO);
    for block in rx {
        let (label, ntx, spec) = (block.label.clone(), block.txs.len(), block.spec);
        println!("===== replay block {label} ({ntx} txs, spec {spec:?}) =====");
        // The sequential reference runs first inside the comparison. A real parallel divergence
        // `assert_eq!`-panics (the offending account/value is printed); `catch_unwind` turns that
        // into a fail-fast exit. A `SequentialFailed` outcome means the block itself can't be
        // replayed (not grevm's fault) — skip it and continue.
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            execute::compare_evm_execute_with_env(
                block.db,
                block.txs,
                false,
                block.cfg,
                block.block_env,
                Default::default(),
            )
        }));
        match outcome {
            Ok(execute::ReplayOutcome::Ok { sequential, parallel }) => {
                replayed += 1;
                total_seq += sequential;
                total_par += parallel;
                println!(
                    "  block {label}: OK (execution only: sequential {sequential:?}, \
                     parallel {parallel:?})"
                );
            }
            Ok(execute::ReplayOutcome::SequentialFailed(e)) => {
                skipped += 1;
                eprintln!("  block {label}: SKIP (sequential reference failed: {e})");
            }
            Err(_) => {
                eprintln!(
                    "\nFAILED at block {label}: grevm parallel result != sequential \
                     (see the assertion above for the diverging account/value)"
                );
                std::process::exit(1);
            }
        }
    }

    fetcher.join().map_err(|_| "prefetch thread panicked")??;

    println!("Done: {replayed} blocks passed, {skipped} skipped (unreplayable fixtures)");
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
        if !block.is_null() && filter.matches(&block) {
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
    let spec =
        spec_for_timestamp(u64::from_str_radix(ts.trim_start_matches("0x"), 16).unwrap_or(0))
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
    mainnet::accumulate_prestate(&mut pre_state, &trace)?;
    // prestateTracer omits EIP-7702 delegation targets' code; fetch them (cached across blocks).
    let parent = format!("0x{:x}", number.saturating_sub(1));
    rpc.supplement_delegations_cached(&txs, &mut pre_state, &parent, &mut caches.delegates)?;
    if let Some(dir) = save_dir {
        mainnet::write_mainnet_block(dir, &bf, &txs, &pre_state)?;
    }
    Ok(MainnetBlock::from_fixtures(number.to_string(), &bf, &txs, &pre_state))
}
