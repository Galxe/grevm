//! Discover EIP-7702 (type-4) mainnet blocks over JSON-RPC and replay each one through grevm's
//! parallel-vs-sequential check — all in a single process.
//!
//! Blocks are scanned **upward** from `start_block` toward the chain head. A background thread
//! discovers and fetches the *next* 7702 block while the main thread replays the *current* one
//! (`sync_channel(1)` keeps the fetcher exactly one block ahead), so network I/O overlaps with
//! execution without spawning a new process per block.
//!
//! Discovery scans the chain directly via the RPC (no etherscan scraping / API key needed); a
//! `debug`-namespace endpoint (for `prestateTracer`) is required.
//!
//! Usage:
//! ```text
//! cargo run --bin replay_7702 --features tools -- <rpc_url> [start_block] [count] [out_dir]
//! ```
//! - `start_block` — block to start scanning upward from. Default: the mainnet EIP-7702 activation
//!   block ([`PECTRA_BLOCK`]).
//! - `count`       — how many 7702 blocks to replay. Default: all of them up to the chain head.
//! - `out_dir`     — optional. If given, each replayed block's fixture is written to
//!   `<out_dir>/<number>/` (same format as `fetch_block`). Omitted ⇒ fetched in memory only,
//!   nothing is written to disk.
//!
//! Blocks are validated as they arrive. On the first divergence (grevm parallel result !=
//! sequential) the offending block is reported and the process exits non-zero.

mod rpc;

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
};

use grevm::test_utils::common::{
    execute,
    mainnet::{self, BlockFixture, MainnetBlock, PreState, TxFixture, spec_for_timestamp},
};
use rpc::{Rpc, parse_block_number};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Mainnet block at which EIP-7702 (the Pectra hardfork) activated, 2025-05-07 — the default start
/// block. Hardcoded for Ethereum mainnet; pass an explicit `start_block` for other chains.
const PECTRA_BLOCK: u64 = 22_431_084;

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: cargo run --bin replay_7702 --features tools -- \
             <rpc_url> [start_block] [count] [out_dir]"
        );
        std::process::exit(1);
    }
    let rpc_url = args[1].clone();
    let start_arg = args.get(2).cloned();
    let count: Option<usize> = args.get(3).map(|s| s.parse()).transpose()?;
    let save_dir: Option<PathBuf> = args.get(4).map(PathBuf::from);

    // Force the parallel path even for small blocks. SAFETY: set before any thread is spawned and
    // before any execution reads it, so there is no concurrent getenv/setenv.
    if std::env::var_os("GREVM_MIN_PARALLEL_TXS").is_none() {
        unsafe { std::env::set_var("GREVM_MIN_PARALLEL_TXS", "0") };
    }

    let rpc = Rpc::new(rpc_url.clone());
    let head = rpc.head_block()?;
    let start = match start_arg {
        Some(s) => parse_block_number(&s)?,
        None => PECTRA_BLOCK,
    };
    match count {
        Some(c) => println!("Replaying up to {c} EIP-7702 block(s) from {start} (head {head})"),
        None => println!("Replaying ALL EIP-7702 blocks from {start} up to head {head}"),
    }
    if let Some(dir) = &save_dir {
        println!("Persisting each block's fixture under {}", dir.display());
    }

    // Prefetch thread: discover + fetch the next 7702 block while the main thread replays the
    // current one. `sync_channel(1)` keeps it exactly one block ahead.
    let (tx, rx) = mpsc::sync_channel::<MainnetBlock>(1);
    let fetcher = thread::spawn(move || -> Result<(), Error> {
        let rpc = Rpc::new(rpc_url);
        let chain_id = rpc.chain_id()?;
        let mut from = start;
        let mut produced = 0usize;
        loop {
            if count.is_some_and(|c| produced >= c) {
                break;
            }
            match next_7702(&rpc, from, head, chain_id, save_dir.as_deref())? {
                Some(block) => {
                    let next = block.number + 1;
                    if tx.send(block).is_err() {
                        break; // receiver gone
                    }
                    from = next;
                    produced += 1;
                }
                None => break, // reached head, no more 7702 blocks
            }
        }
        Ok(())
    });

    let mut replayed = 0usize;
    let mut skipped = 0usize;
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
            Ok(execute::ReplayOutcome::Ok) => {
                replayed += 1;
                println!("  block {label}: OK");
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
    Ok(())
}

/// Scan upward from `from` to `head` (inclusive), returning the first block (built in memory) that
/// contains at least one EIP-7702 (type-4) transaction, or `None` if none remain. If `save_dir` is
/// set, the block's fixture is also written there.
fn next_7702(
    rpc: &Rpc,
    from: u64,
    head: u64,
    chain_id: u64,
    save_dir: Option<&Path>,
) -> Result<Option<MainnetBlock>, Error> {
    let mut bn = from;
    while bn <= head {
        let hex = format!("0x{bn:x}");
        let block = rpc.call("eth_getBlockByNumber", json!([hex, true]))?;
        if !block.is_null() && has_eip7702(&block) {
            return Ok(Some(build_block(rpc, bn, chain_id, &block, save_dir)?));
        }
        bn += 1;
    }
    Ok(None)
}

fn has_eip7702(block: &Value) -> bool {
    block
        .get("transactions")
        .and_then(Value::as_array)
        .is_some_and(|txs| txs.iter().any(|t| t.get("type").and_then(Value::as_str) == Some("0x4")))
}

/// Build a [`MainnetBlock`] in memory from an already-fetched `eth_getBlockByNumber` result plus a
/// fresh `prestateTracer` trace.
fn build_block(
    rpc: &Rpc,
    number: u64,
    chain_id: u64,
    block: &Value,
    save_dir: Option<&Path>,
) -> Result<MainnetBlock, Error> {
    let ts = block.get("timestamp").and_then(Value::as_str).unwrap_or("0x0");
    let spec =
        spec_for_timestamp(u64::from_str_radix(ts.trim_start_matches("0x"), 16).unwrap_or(0))
            .to_string();
    let mut bf = BlockFixture::from_rpc(number, chain_id, spec, block)?;
    bf.block_hashes =
        rpc.fetch_block_hashes(number.saturating_sub(256), number.saturating_sub(1))?;
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
    // prestateTracer omits EIP-7702 delegation targets' code; fetch them at the parent block.
    let parent = format!("0x{:x}", number.saturating_sub(1));
    rpc.supplement_delegations(&txs, &mut pre_state, &parent)?;
    if let Some(dir) = save_dir {
        mainnet::write_mainnet_block(dir, &bf, &txs, &pre_state)?;
    }
    Ok(MainnetBlock::from_fixtures(number.to_string(), &bf, &txs, &pre_state))
}
