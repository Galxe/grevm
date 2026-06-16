#![allow(missing_docs)]

//! Benchmark grevm's parallel scheduler against revm's sequential executor on the merged "big
//! block" fixtures in `test_data/con_eth_blocks/` (produced by the `fetch_continuous` example).
//!
//! Each fixture is first checked for correctness (parallel == sequential) once, then timed. If no
//! fixtures are present the benchmark skips cleanly.

use std::sync::Arc;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use grevm::{
    ParallelState, ParallelTakeBundle, Scheduler,
    test_utils::common::{execute, mainnet},
};
use revm_database::states::bundle_state::BundleRetention;

#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn bench_continuous(c: &mut Criterion) {
    let blocks = mainnet::load_continuous_blocks();
    if blocks.is_empty() {
        eprintln!(
            "no big-block fixtures under {} — skipping (build one with the `fetch_continuous` \
             example)",
            mainnet::continuous_blocks_root().display()
        );
        return;
    }

    for block in blocks {
        let label = format!("con_eth_block {} ({} txs)", block.number, block.txs.len());

        // One-time correctness gate: parallel must equal sequential on this big block.
        execute::compare_evm_execute_with_env(
            block.db.clone(),
            block.txs.clone(),
            false,
            block.cfg.clone(),
            block.block_env.clone(),
            Default::default(),
        );

        let db = Arc::new(block.db);
        let txs = Arc::new(block.txs);
        let cfg = block.cfg;
        let env = block.block_env;

        let mut group = c.benchmark_group(label);
        group.bench_function("Grevm Parallel", |b| {
            b.iter(|| {
                let state = ParallelState::new(db.clone(), true, false);
                let executor = Scheduler::new(
                    black_box(cfg.clone()),
                    black_box(env.clone()),
                    black_box(txs.clone()),
                    state,
                    false,
                    None,
                );
                executor.parallel_execute(None).unwrap();
                let (_, mut inner) = executor.take_result_and_state();
                let _ = inner.parallel_take_bundle(BundleRetention::Reverts);
            })
        });
        group.bench_function("Origin Sequential", |b| {
            b.iter(|| {
                let _ =
                    execute::execute_revm_sequential(db.clone(), cfg.clone(), env.clone(), &*txs)
                        .unwrap();
            })
        });
        group.finish();
    }
}

criterion_group!(benches, bench_continuous);
criterion_main!(benches);
