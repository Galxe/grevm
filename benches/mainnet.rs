#![allow(missing_docs)]

#[path = "../tests/common/mod.rs"]
pub mod common;

use std::sync::Arc;

use crate::common::execute_revm_sequential;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use grevm::{Scheduler, StateAsyncCommit};

fn benchmark_mainnet(c: &mut Criterion) {
    let db_latency_us = std::env::var("DB_LATENCY_US").map(|s| s.parse().unwrap()).unwrap_or(0);

    common::for_each_block_from_disk(|env, txs, mut db| {
        db.latency_us = db_latency_us;
        let number = env.env.block.number;
        let num_txs = txs.len();
        let mut group = c.benchmark_group(format!("Block {number}({num_txs} txs)"));

        let txs = Arc::new(txs);
        let db = Arc::new(db);

        group.bench_function("Origin Sequential", |b| {
            b.iter(|| {
                execute_revm_sequential(db.clone(), env.spec_id(), env.env.as_ref().clone(), &*txs)
                    .unwrap();
            })
        });

        group.bench_function("Grevm Parallel", |b| {
            b.iter(|| {
                let mut executor = Scheduler::new(
                    black_box(env.spec_id()),
                    black_box(env.env.as_ref().clone()),
                    black_box(txs.clone()),
                    black_box(db.clone()),
                    true,
                );
                executor.parallel_execute(None).unwrap();
            })
        });
    });
}

criterion_group!(benches, benchmark_mainnet);
criterion_main!(benches);
