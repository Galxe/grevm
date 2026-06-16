//! Download a real Ethereum mainnet block's *execution environment* over JSON-RPC and write it as
//! a grevm replay fixture under `test_data/mainnet_blocks/<number>/`.
//!
//! It issues three calls against the node (a paid/archive endpoint with the `debug` namespace
//! enabled is required):
//! - `eth_chainId`              — chain id for the [`CfgEnv`](revm_context::CfgEnv).
//! - `eth_getBlockByNumber`     — block header + full transaction objects.
//! - `debug_traceBlockByNumber` — per-tx `prestateTracer` read sets, merged ("first-seen wins")
//!   into the block's opening state.
//!
//! The JSON-RPC-response → fixture mapping lives in [`grevm::test_utils::common::mainnet`]; this
//! binary is just the HTTP transport + CLI.
//!
//! Usage:
//! ```text
//! cargo run --bin fetch_block --features tools -- <block> <rpc_url> [spec] [out_dir]
//! ```
//! - `block`   : decimal (`25323281`) or hex (`0x1826711`).
//! - `rpc_url` : JSON-RPC HTTP endpoint.
//! - `spec`    : optional hardfork override (e.g. `Prague`); inferred from the timestamp otherwise.
//! - `out_dir` : optional output root (default `test_data/mainnet_blocks`).
//!
//! Then replay with: `GREVM_MIN_PARALLEL_TXS=0 cargo test --features test-utils --test mainnet`.

mod rpc;

use std::path::PathBuf;

use grevm::test_utils::common::mainnet::{
    self, BlockFixture, PreState, TxFixture, spec_for_timestamp, write_mainnet_block,
};
use rpc::{Rpc, parse_block_number};
use serde_json::{Value, json};

type Error = Box<dyn std::error::Error + Send + Sync>;

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: cargo run --bin fetch_block --features tools -- \
             <block_number|0xhex> <rpc_url> [spec] [out_dir]"
        );
        std::process::exit(1);
    }
    let block_number = parse_block_number(&args[1])?;
    let rpc = Rpc::new(args[2].clone());
    let spec_override = args.get(3).cloned();
    // Default to the same dir the replay test reads (`GREVM_MAINNET_BLOCKS`, else the standard
    // dir).
    let out_dir = args.get(4).map(PathBuf::from).unwrap_or_else(mainnet::mainnet_blocks_root);

    let block_hex = format!("0x{block_number:x}");
    println!("Fetching block {block_number} ({block_hex}) from {}", rpc.url());

    let chain_id = rpc.chain_id()?;

    // 1) header + full txs
    let block = rpc.call("eth_getBlockByNumber", json!([block_hex, true]))?;
    if block.is_null() {
        return Err(format!("block {block_hex} not found").into());
    }
    let spec_id = spec_override.unwrap_or_else(|| {
        let ts = block.get("timestamp").and_then(Value::as_str).unwrap_or("0x0");
        spec_for_timestamp(u64::from_str_radix(ts.trim_start_matches("0x"), 16).unwrap_or(0))
            .to_string()
    });
    let mut block_fixture = BlockFixture::from_rpc(block_number, chain_id, spec_id, &block)?;
    // prestateTracer can't report block hashes (not account state); fetch the last 256 for
    // BLOCKHASH.
    block_fixture.block_hashes =
        rpc.fetch_block_hashes(block_number.saturating_sub(256), block_number.saturating_sub(1))?;

    let txs: Vec<TxFixture> = block
        .get("transactions")
        .and_then(Value::as_array)
        .map(|v| v.iter().map(TxFixture::from_rpc).collect::<Result<_, _>>())
        .transpose()?
        .unwrap_or_default();
    println!("  {} transactions, hardfork = {}", txs.len(), block_fixture.spec_id);

    // 2) full prestate read set (default tracer, NOT diffMode), merged into the opening state
    let trace =
        rpc.call("debug_traceBlockByNumber", json!([block_hex, { "tracer": "prestateTracer" }]))?;
    let mut pre_state = PreState::new();
    mainnet::accumulate_prestate(&mut pre_state, &trace)?;
    // prestateTracer omits EIP-7702 delegation targets' code; fetch them at the parent block.
    let parent = format!("0x{:x}", block_number.saturating_sub(1));
    let n = rpc.supplement_delegations(&txs, &mut pre_state, &parent)?;
    println!(
        "  merged opening state: {} accounts (+{n} EIP-7702 delegate targets)",
        pre_state.len()
    );

    let dir = write_mainnet_block(&out_dir, &block_fixture, &txs, &pre_state)?;
    println!("Wrote fixture to {}", dir.display());
    println!(
        "Replay with: GREVM_MIN_PARALLEL_TXS=0 GREVM_MAINNET_BLOCKS={} \
         cargo test --features test-utils --test mainnet",
        out_dir.display()
    );
    Ok(())
}
