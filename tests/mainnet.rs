#![allow(missing_docs)]

mod common;
use std::sync::Arc;

use common::storage::InMemoryDB;
use grevm::Scheduler;
use metrics_util::debugging::{DebugValue, DebuggingRecorder};
use revm_primitives::{EnvWithHandlerCfg, TxEnv};

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
        let mut executor = Scheduler::new(env.spec_id(), *env.env, txs, db, with_hints);
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
        let result = executor.with_commiter(|commiter| commiter.take_result());
        let bundle = executor.with_commiter(|commiter| commiter.take_bundle());
        (result, bundle)
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
