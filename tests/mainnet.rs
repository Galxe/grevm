#![allow(missing_docs)]

mod common;

use std::sync::Arc;

use common::storage::InMemoryDB;
use grevm::{ParallelState, ParallelTakeBundle, Scheduler};
use metrics_util::debugging::{DebugValue, DebuggingRecorder};

const GIGA_GAS: u64 = 1_000_000_000;

/// Return gas used
fn test_execute(
    env: EnvWithHandlerCfg,
    txs: Vec<TxEnv>,
    db: InMemoryDB,
    dump_transition: bool,
) -> u64 {
    let txs = Arc::new(txs);
    let db = Arc::new(db);

    let reth_result =
        common::execute_revm_sequential(db.clone(), env.spec_id(), env.env.as_ref().clone(), &*txs)
            .unwrap();

    // create registry for metrics
    let recorder = DebuggingRecorder::new();
    let with_hints = std::env::var("WITH_HINTS").map_or(false, |s| s.parse().unwrap());
    let parallel_result = metrics::with_local_recorder(&recorder, || {
        let state = ParallelState::new(db.clone(), true, true);
        let mut executor = Scheduler::new(env.spec_id(), *env.env, txs, state, with_hints);
        executor.parallel_execute(None).unwrap();

        let snapshot = recorder.snapshotter().snapshot();
        for (key, _, _, value) in snapshot.into_vec() {
            let value = match value {
                DebugValue::Counter(v) => v as usize,
                DebugValue::Gauge(v) => v.0 as usize,
                DebugValue::Histogram(v) => v.last().cloned().map_or(0, |ov| ov.0 as usize),
            };
            println!("metrics: {} => value: {:?}", key.key().name(), value);
        }
        let (results, mut state) = executor.take_result_and_state();
        (results, state.parallel_take_bundle(BundleRetention::Reverts))
    });

    common::compare_execution_result(&reth_result.0, &parallel_result.0);
    common::compare_bundle_state(&reth_result.1, &parallel_result.1);
    reth_result.0.iter().map(|r| r.gas_used()).sum()
}

#[test]
fn mainnet() {
    let dump_transition = std::env::var("DUMP_TRANSITION").is_ok();
    if let Ok(block_number) = std::env::var("BLOCK_NUMBER").map(|s| s.parse().unwrap()) {
        // Test a specific block
        let bytecodes = common::load_bytecodes_from_disk();
        let (env, txs, mut db) = common::load_block_from_disk(block_number);
        if db.bytecodes.is_empty() {
            // Use the global bytecodes if the block doesn't have its own
            db.bytecodes = bytecodes.clone();
        }
        test_execute(env, txs, db, dump_transition);
        return;
    }

    common::for_each_block_from_disk(|env, txs, db| {
        let number = env.env.block.number;
        let num_txs = txs.len();
        println!("Test Block {number}");
        let gas_used = test_execute(env, txs, db, dump_transition);
        println!("Test Block {number} done({num_txs} txs, {gas_used} gas)");
    });
}

fn one_gigagas() {
    let continuous_blocks = String::from("19126588_19127588");
    if !common::continuous_blocks_exist(continuous_blocks.clone()) {
        panic!("No test data");
    }

    let (block_env, mut db) = common::load_continuous_blocks(continuous_blocks, Some(100));
    let bytecodes = common::load_bytecodes_from_disk();
    if db.bytecodes.is_empty() {
        db.bytecodes = bytecodes;
    }

    let mut smallest_base_fee = 0;
    for i in 0..block_env.len() {
        if block_env[i].0.block.basefee < block_env[smallest_base_fee].0.block.basefee {
            smallest_base_fee = i;
        }
    }
    let env = block_env[smallest_base_fee].0.clone();

    let mut passed_txs = vec![];
    let mut total_gas = 0;
    let mut num_blocks = 0;
    let mut sequential_state =
        StateBuilder::new().with_bundle_update().with_database_ref(db).build();
    {
        let mut evm = EvmBuilder::default()
            .with_db(&mut sequential_state)
            .with_spec_id(env.spec_id())
            .with_env(Box::new(env.env.as_ref().clone()))
            .build();
        for (_, txs, _) in block_env.clone() {
            num_blocks += 1;
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
    }

    common::dump_block_env(
        &env,
        &passed_txs,
        &sequential_state.cache,
        sequential_state.transition_state.as_ref().unwrap(),
        &sequential_state.block_hashes,
    );
}

#[test]
fn continuous_blocks() {
    let continuous_blocks =
        std::env::var("CONTINUOUS_BLOCKS").unwrap_or(String::from("19126588_19127588"));
    if !common::continuous_blocks_exist(continuous_blocks.clone()) {
        return;
    }

    let (block_env, mut db) = common::load_continuous_blocks(continuous_blocks, Some(70));
    let bytecodes = common::load_bytecodes_from_disk();
    if db.bytecodes.is_empty() {
        db.bytecodes = bytecodes;
    }
    let with_hints = std::env::var("WITH_HINTS").map_or(false, |s| s.parse().unwrap());
    let db = Arc::new(db);

    // parallel execute
    let mut total_gas = 0;
    let mut num_blocks = 0;
    let mut state = ParallelState::new(db.clone(), true, false);
    let mut parallel_results = vec![];
    for (env, txs, post) in block_env.clone() {
        let mut executor =
            Scheduler::new(env.spec_id(), *env.env, Arc::new(txs), state, with_hints);
        executor.parallel_execute(None).unwrap();
        let (results, mut inner_state) = executor.take_result_and_state();
        total_gas += results.iter().map(|r| r.gas_used()).sum::<u64>();
        inner_state.increment_balances(post).unwrap();
        state = inner_state;
        parallel_results.extend(results);
        num_blocks += 1;
        if total_gas > GIGA_GAS {
            println!(
                "Execute {} blocks({} txs) with {} gas",
                num_blocks,
                parallel_results.len(),
                total_gas
            );
            break;
        }
    }
    let parallel_bundle = state.parallel_take_bundle(BundleRetention::Reverts);

    // sequential execute
    let mut sequential_state =
        StateBuilder::new().with_bundle_update().with_database_ref(db).build();
    let mut executed_blocks = 0;
    let mut sequential_results = vec![];
    for (env, txs, post) in block_env {
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
                sequential_results.push(result_and_state.result);
            }
        }
        sequential_state.increment_balances(post).unwrap();
        executed_blocks += 1;
        if executed_blocks >= num_blocks {
            break;
        }
    }
    sequential_state.merge_transitions(BundleRetention::Reverts);
    let sequential_bundle = sequential_state.take_bundle();

    common::compare_execution_result(&sequential_results, &parallel_results);
    common::compare_bundle_state(&sequential_bundle, &parallel_bundle);
}
