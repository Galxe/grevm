#![allow(missing_docs)]

#[path = "../tests/common/mod.rs"]
pub mod common;

use criterion::{Criterion, criterion_group, criterion_main};
use grevm::{ParallelState, ParallelTakeBundle, Scheduler};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use rand::Rng;
use revm::{EvmBuilder, StateBuilder, db::states::bundle_state::BundleRetention};
use revm_primitives::db::DatabaseCommit;
use std::{cmp::max, fs, sync::Arc, time::Instant};

const TEST_DATA_DIR: &str = "test_data";
const GIGA_GAS: u64 = 1_000_000_000;

fn bench_1gigagas(c: &mut Criterion) {
    let db_latency_us = std::env::var("DB_LATENCY_US").map(|s| s.parse().unwrap()).unwrap_or(0);
    let with_hints = std::env::var("WITH_HINTS").map_or(false, |s| s.parse().unwrap());

    let continuous_blocks = String::from("19126588_19127588");
    if !common::continuous_blocks_exist(continuous_blocks.clone()) {
        panic!("No test data");
    }

    let (block_env, mut db) = common::load_continuous_blocks(continuous_blocks, Some(100));
    let bytecodes = common::load_bytecodes_from_disk();
    if db.bytecodes.is_empty() {
        db.bytecodes = bytecodes;
    }
    db.latency_us = db_latency_us;
    let db = Arc::new(db);

    let mut smallest_base_fee = 0;
    for i in 0..block_env.len() {
        if block_env[i].0.block.basefee < block_env[smallest_base_fee].0.block.basefee {
            smallest_base_fee = i;
        }
    }
    let env = block_env[smallest_base_fee].0.clone();

    let mut passed_txs = vec![];
    let mut total_gas = 0;
    let mut sequential_state =
        StateBuilder::new().with_bundle_update().with_database_ref(db.clone()).build();
    let mut evm = EvmBuilder::default()
        .with_db(&mut sequential_state)
        .with_spec_id(env.spec_id())
        .with_env(Box::new(env.env.as_ref().clone()))
        .build();
    for (_, txs, _) in block_env.clone() {
        for tx in txs {
            *evm.tx_mut() = tx.clone();
            match evm.transact() {
                Ok(result_and_state) => {
                    evm.db_mut().commit(result_and_state.state);
                    passed_txs.push(tx);
                    total_gas += result_and_state.result.gas_used();
                    if total_gas >= GIGA_GAS {
                        break;
                    }
                }
                Err(_) => {}
            }
        }
    }

    let mut group =
        c.benchmark_group(format!("Large Block({} txs) with 1Gigagas", passed_txs.len()));
    let mut iter_loop = 0;
    let report_metrics = rand::thread_rng().gen_range(0..10);
    let txs = Arc::new(passed_txs);
    group.bench_function("Grevm Parallel", |b| {
        b.iter(|| {
            let recorder = DebuggingRecorder::new();
            let state = ParallelState::new(db.clone(), true, false);
            metrics::with_local_recorder(&recorder, || {
                let env = env.clone();
                let mut executor =
                    Scheduler::new(env.spec_id(), *env.env, txs.clone(), state, with_hints);
                executor.parallel_execute(None).unwrap();
                let (_, mut inner_state) = executor.take_result_and_state();
                let _ = inner_state.parallel_take_bundle(BundleRetention::Reverts);
            });
            if iter_loop == report_metrics {
                let snapshot = recorder.snapshotter().snapshot();
                println!("\n>>>> Large Block metrics: <<<<");
                for (key, _, _, value) in snapshot.into_vec() {
                    let value = match value {
                        DebugValue::Counter(v) => v as usize,
                        DebugValue::Gauge(v) => v.0 as usize,
                        DebugValue::Histogram(v) => {
                            let mut s: Vec<usize> =
                                v.clone().into_iter().map(|ov| ov.0 as usize).collect();
                            s.sort();
                            s.get(s.len() / 2).cloned().unwrap_or(0)
                        }
                    };
                    println!("{} => {:?}", key.key().name(), value);
                }
            }
            iter_loop += 1;
        })
    });

    group.bench_function("Origin Sequential", |b| {
        b.iter(|| {
            common::execute_revm_sequential(
                db.clone(),
                env.spec_id(),
                env.env.as_ref().clone(),
                &*txs,
            )
            .unwrap();
        })
    });
}

fn bench_continuous_mainnet(c: &mut Criterion) {
    let db_latency_us = std::env::var("DB_LATENCY_US").map(|s| s.parse().unwrap()).unwrap_or(0);
    let with_hints = std::env::var("WITH_HINTS").map_or(false, |s| s.parse().unwrap());

    let bytecodes = common::load_bytecodes_from_disk();
    for block_path in fs::read_dir(format!("{TEST_DATA_DIR}/con_eth_blocks")).unwrap() {
        let block_path = block_path.unwrap().path();
        let block_range = block_path.file_name().unwrap().to_str().unwrap();
        let mut group = c.benchmark_group(format!("Block Range {block_range}"));
        let start = Instant::now();
        println!("Start to load dump env for block range {}", block_range);
        let (block_env, mut db) = common::load_continuous_blocks(block_range.to_string(), None);
        println!(
            "Load dump env for block range {} with {}s",
            block_range,
            start.elapsed().as_secs()
        );
        if db.bytecodes.is_empty() {
            db.bytecodes = bytecodes.clone();
        }
        db.latency_us = db_latency_us;
        let db = Arc::new(db);

        let mut iter_loop = 0;
        let report_metrics = rand::thread_rng().gen_range(0..10);
        group.bench_function("Grevm Parallel", |b| {
            b.iter(|| {
                let recorder = DebuggingRecorder::new();
                let mut state = ParallelState::new(db.clone(), true, false);
                metrics::with_local_recorder(&recorder, || {
                    for (env, txs, post) in block_env.clone() {
                        let mut executor = Scheduler::new(
                            env.spec_id(),
                            *env.env,
                            Arc::new(txs),
                            state,
                            with_hints,
                        );
                        executor.parallel_execute(None).unwrap();
                        let (_, mut inner_state) = executor.take_result_and_state();
                        inner_state.increment_balances(post).unwrap();
                        state = inner_state;
                    }
                    let _ = state.parallel_take_bundle(BundleRetention::Reverts);
                });
                if iter_loop == report_metrics {
                    let snapshot = recorder.snapshotter().snapshot();
                    println!("\n>>>> Block Range {} P50 metrics: <<<<", block_range);
                    for (key, _, _, value) in snapshot.into_vec() {
                        let value = match value {
                            DebugValue::Counter(v) => v as usize,
                            DebugValue::Gauge(v) => v.0 as usize,
                            DebugValue::Histogram(v) => {
                                let mut s: Vec<usize> =
                                    v.clone().into_iter().map(|ov| ov.0 as usize).collect();
                                s.sort();
                                s.get(s.len() / 2).cloned().unwrap_or(0)
                            }
                        };
                        println!("{} => {:?}", key.key().name(), value);
                    }
                }
                iter_loop += 1;
            })
        });

        group.bench_function("Origin Sequential", |b| {
            b.iter(|| {
                let mut sequential_state =
                    StateBuilder::new().with_bundle_update().with_database_ref(db.clone()).build();
                for (env, txs, post) in block_env.clone() {
                    {
                        let mut evm = EvmBuilder::default()
                            .with_db(&mut sequential_state)
                            .with_spec_id(env.spec_id())
                            .with_env(Box::new(env.env.as_ref().clone()))
                            .build();
                        for tx in txs {
                            *evm.tx_mut() = tx.clone();
                            let result_and_state = evm.transact().unwrap();
                            evm.db_mut().commit(result_and_state.state);
                        }
                    }
                    sequential_state.increment_balances(post).unwrap();
                    sequential_state.merge_transitions(BundleRetention::Reverts);
                    let _ = sequential_state.take_bundle();
                }
            })
        });
    }
}

fn bench_blocks(c: &mut Criterion) {
    bench_1gigagas(c);
    bench_continuous_mainnet(c);
}

criterion_group!(benches, bench_blocks);
criterion_main!(benches);
