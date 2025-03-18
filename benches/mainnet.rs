#![allow(missing_docs)]

#[path = "../tests/common/mod.rs"]
pub mod common;

use std::sync::Arc;

use crate::common::execute_revm_sequential;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use grevm::{ParallelState, Scheduler};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use rand::Rng;

fn benchmark_mainnet(c: &mut Criterion) {
    let db_latency_us = std::env::var("DB_LATENCY_US").map(|s| s.parse().unwrap()).unwrap_or(0);
    let with_hints = std::env::var("WITH_HINTS").map_or(false, |s| s.parse().unwrap());

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

        let mut iter_loop = 0;
        let report_metrics = rand::thread_rng().gen_range(0..10);
        group.bench_function("Grevm Parallel", |b| {
            b.iter(|| {
                let recorder = DebuggingRecorder::new();
                let state = ParallelState::new(db.clone(), true, false);
                metrics::with_local_recorder(&recorder, || {
                    let mut executor = Scheduler::new(
                        black_box(env.spec_id()),
                        black_box(env.env.as_ref().clone()),
                        black_box(txs.clone()),
                        black_box(state),
                        with_hints,
                    );
                    executor.parallel_execute(None).unwrap();
                });
                if iter_loop == report_metrics {
                    let snapshot = recorder.snapshotter().snapshot();
                    println!("\n>>>> Block {}({} txs) metrics: <<<<", number, num_txs);
                    for (key, _, _, value) in snapshot.into_vec() {
                        let value = match value {
                            DebugValue::Counter(v) => v as usize,
                            DebugValue::Gauge(v) => v.0 as usize,
                            DebugValue::Histogram(v) => {
                                v.last().cloned().map_or(0, |ov| ov.0 as usize)
                            }
                        };
                        println!("{} => {:?}", key.key().name(), value);
                    }
                }
                iter_loop += 1;
            })
        });
    });
}

criterion_group!(benches, benchmark_mainnet);
criterion_main!(benches);
