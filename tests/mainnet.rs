#![allow(missing_docs)]

//! Replay real Ethereum mainnet blocks through grevm and check the parallel scheduler against a
//! sequential revm reference.
//!
//! - [`replay_mainnet_blocks`] replays single-block fixtures under `test_data/mainnet_blocks/`
//!   (produced by the `fetch_block` binary).
//! - [`replay_continuous_blocks`] replays merged "big block" fixtures under
//!   `test_data/con_eth_blocks/` (produced by the `fetch_continuous` binary).
//!
//! Both load from a git submodule that may be absent in a fresh checkout; the test then skips
//! instead of failing. Select a single fixture with an environment variable:
//! ```text
//! GREVM_MAINNET_BLOCK=25323281   cargo test --features test-utils --test mainnet replay_mainnet_blocks
//! GREVM_CONTINUOUS_RANGE=25326115_25326124 cargo test --features test-utils --test mainnet replay_continuous_blocks
//! ```
//!
//! Set `GREVM_MIN_PARALLEL_TXS=0` so even small blocks (real blocks are frequently < 64 txs)
//! exercise the parallel path rather than falling back to sequential:
//! ```text
//! GREVM_MIN_PARALLEL_TXS=0 cargo test --features test-utils --test mainnet
//! ```
//!
//! Oracle: grevm parallel == revm sequential on the same opening state and transactions. This is
//! independent of mainnet block-level system calls (EIP-4788/2935/7002/7251) and withdrawals,
//! which are not part of the transaction list and are intentionally out of scope here.

use std::path::Path;

use grevm::test_utils::common::{execute, mainnet};

/// Replay single-block fixtures. Set `GREVM_MAINNET_BLOCK=<number>` to replay just one.
#[test]
fn replay_mainnet_blocks() {
    let mut blocks = mainnet::load_mainnet_blocks();
    if let Ok(only) = std::env::var("GREVM_MAINNET_BLOCK") {
        blocks.retain(|b| b.label == only || b.number.to_string() == only);
        assert!(
            !blocks.is_empty(),
            "GREVM_MAINNET_BLOCK={only}: no such fixture under {}",
            mainnet::mainnet_blocks_root().display()
        );
    }
    replay(blocks, &mainnet::mainnet_blocks_root());
}

/// Replay merged "big block" fixtures. Set `GREVM_CONTINUOUS_RANGE=<start>_<end>` to replay just
/// one.
#[test]
fn replay_continuous_blocks() {
    let mut blocks = mainnet::load_continuous_blocks();
    if let Ok(only) = std::env::var("GREVM_CONTINUOUS_RANGE") {
        blocks.retain(|b| b.label == only);
        assert!(
            !blocks.is_empty(),
            "GREVM_CONTINUOUS_RANGE={only}: no such fixture under {}",
            mainnet::continuous_blocks_root().display()
        );
    }
    replay(blocks, &mainnet::continuous_blocks_root());
}

fn replay(blocks: Vec<mainnet::MainnetBlock>, root: &Path) {
    if blocks.is_empty() {
        eprintln!("no fixtures under {} — skipping (fetch some first)", root.display());
        return;
    }
    for block in blocks {
        let label = block.label.clone();
        println!("replaying {label} ({} txs, spec {:?})", block.txs.len(), block.spec);
        match execute::compare_evm_execute_with_env(
            block.db,
            block.txs,
            false,
            block.cfg,
            block.block_env,
            Default::default(),
        ) {
            execute::ReplayOutcome::Ok { .. } => {}
            // The sequential reference itself couldn't execute the block (unreplayable fixture) —
            // not a grevm divergence, so skip rather than fail the test.
            execute::ReplayOutcome::SequentialFailed(e) => {
                eprintln!("  skip {label}: sequential reference failed ({e})");
            }
        }
    }
}
