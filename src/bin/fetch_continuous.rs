//! Fetch a contiguous range of mainnet blocks and merge them into a single large "big block"
//! fixture under `test_data/con_eth_blocks/<start>_<end>/`, for benchmarking grevm's parallelism
//! on a single oversized block.
//!
//! All blocks' transactions are concatenated in order and their `prestateTracer` read sets merged
//! ("first-seen wins") into one opening state. Because the merged block runs under a *single*
//! block environment, some transactions become invalid (wrong nonce after reordering, basefee
//! mismatch, insufficient funds, …); we pick the range's lowest-basefee block as the shared
//! environment (to maximize survivors) and then filter out every transaction that can't execute
//! (grevm aborts a block on the first transaction error, so the stored block must be clean).
//!
//! Usage:
//! ```text
//! cargo run --bin fetch_continuous --features tools -- <start> <count> <rpc_url> [out_dir]
//! ```
//! - `start`   : first block number (decimal or `0x` hex).
//! - `count`   : how many consecutive blocks to merge.
//! - `rpc_url` : JSON-RPC HTTP endpoint (debug namespace required).
//! - `out_dir` : optional output root (default `test_data/con_eth_blocks`).
//!
//! Then benchmark with: `cargo bench --features test-utils --bench continuous`.

mod rpc;

use std::path::PathBuf;

use grevm::test_utils::common::{
    execute,
    mainnet::{self, BlockFixture, PreState, TxFixture, spec_for_timestamp, write_block},
};
use revm_context::TxEnv;
use rpc::{Rpc, parse_block_number};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error + Send + Sync>;

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: cargo run --bin fetch_continuous --features tools -- \
             <start_block> <count> <rpc_url> [out_dir]"
        );
        std::process::exit(1);
    }
    let start = parse_block_number(&args[1])?;
    let count: u64 = args[2].parse()?;
    if count == 0 {
        return Err("count must be >= 1".into());
    }
    let rpc = Rpc::new(args[3].clone());
    // Default to the same dir the continuous bench reads (`GREVM_CONTINUOUS_BLOCKS`, else
    // standard).
    let out_dir = args.get(4).map(PathBuf::from).unwrap_or_else(mainnet::continuous_blocks_root);
    let end = start + count - 1;

    println!("Fetching blocks {start}..={end} from {}", rpc.url());
    let chain_id = rpc.chain_id()?;

    let mut all_txs: Vec<TxFixture> = Vec::new();
    let mut pre_state = PreState::new();
    // The shared environment is the range's lowest-basefee block, so the fewest txs get rejected.
    let mut env_block: Option<BlockFixture> = None;

    for bn in start..=end {
        let hex = format!("0x{bn:x}");
        let block = rpc.call("eth_getBlockByNumber", json!([hex, true]))?;
        if block.is_null() {
            return Err(format!("block {hex} not found").into());
        }
        let ts = block.get("timestamp").and_then(Value::as_str).unwrap_or("0x0");
        let spec =
            spec_for_timestamp(u64::from_str_radix(ts.trim_start_matches("0x"), 16).unwrap_or(0))
                .to_string();
        let bf = BlockFixture::from_rpc(bn, chain_id, spec, &block)?;

        let n_tx = if let Some(arr) = block.get("transactions").and_then(Value::as_array) {
            for t in arr {
                all_txs.push(TxFixture::from_rpc(t)?);
            }
            arr.len()
        } else {
            0
        };

        let trace =
            rpc.call("debug_traceBlockByNumber", json!([hex, { "tracer": "prestateTracer" }]))?;
        mainnet::accumulate_prestate(&mut pre_state, &trace)?;

        if env_block.as_ref().is_none_or(|m| bf.basefee < m.basefee) {
            env_block = Some(bf);
        }
        println!("  block {bn}: {n_tx} txs (running total {})", all_txs.len());
    }

    let mut env_block = env_block.expect("at least one block fetched");
    // Block hashes for BLOCKHASH (relative to the shared env block).
    env_block.block_hashes = rpc.fetch_block_hashes(
        env_block.number.saturating_sub(256),
        env_block.number.saturating_sub(1),
    )?;
    // prestateTracer omits EIP-7702 delegation targets' code; fetch them at the opening block.
    let parent = format!("0x{:x}", start.saturating_sub(1));
    let n_deleg = rpc.supplement_delegations(&all_txs, &mut pre_state, &parent)?;
    println!(
        "merged {count} blocks: {} txs, {} accounts (+{n_deleg} EIP-7702 delegate targets); \
         shared env = block {} (basefee {})",
        all_txs.len(),
        pre_state.len(),
        env_block.number,
        env_block.basefee,
    );

    // Drop transactions that can't execute under the single merged environment.
    let mut db = mainnet::pre_state_to_db(&pre_state);
    db.block_hashes = env_block.block_hashes.iter().map(|(k, v)| (*k, *v)).collect();
    let tx_envs: Vec<TxEnv> = all_txs.iter().map(TxFixture::to_tx_env).collect();
    let kept = execute::filter_successful_txs(
        db,
        env_block.to_cfg(),
        env_block.to_block_env(),
        &tx_envs,
        u64::MAX,
    );
    let filtered: Vec<TxFixture> = kept.iter().map(|&i| all_txs[i].clone()).collect();
    println!("kept {}/{} txs after filtering", filtered.len(), all_txs.len());

    let dir = out_dir.join(format!("{start}_{end}"));
    write_block(&dir, &env_block, &filtered, &pre_state)?;
    println!("Wrote big block ({} txs) to {}", filtered.len(), dir.display());
    println!("Benchmark with: cargo bench --features test-utils --bench continuous");
    Ok(())
}
