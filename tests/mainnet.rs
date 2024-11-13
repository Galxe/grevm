#![allow(missing_docs)]

mod common;
use std::sync::Arc;

use alloy_chains::NamedChain;
use alloy_rpc_types::{Block, BlockTransactions};
use common::{compat, storage::InMemoryDB};
use grevm::GrevmScheduler;
use metrics_util::debugging::DebuggingRecorder;
use revm::{
    db::states::bundle_state::BundleRetention,
    primitives::{Env, TxEnv},
};

fn test_execute_alloy(block: Block, db: InMemoryDB) {
    let spec_id = compat::get_block_spec(&block.header);
    let block_env = compat::get_block_env(&block.header);
    let txs: Vec<TxEnv> = match block.transactions {
        BlockTransactions::Full(txs) => txs.into_iter().map(|tx| compat::get_tx_env(tx)).collect(),
        _ => panic!("Missing transaction data"),
    };
    let txs = Arc::new(txs);

    let mut env = Env::default();
    env.cfg.chain_id = NamedChain::Mainnet.into();
    env.block = block_env;

    let db = Arc::new(db);

    let reth_result =
        common::execute_revm_sequential(db.clone(), spec_id, env.clone(), &*txs).unwrap();

    // create registry for metrics
    let recorder = DebuggingRecorder::new();
    let parallel_result = metrics::with_local_recorder(&recorder, || {
        let mut executor = GrevmScheduler::new(spec_id, env, db, txs, None);
        let parallel_result = executor.force_parallel_execute(true, Some(23)).unwrap();

        let snapshot = recorder.snapshotter().snapshot();
        for (key, _, _, value) in snapshot.into_vec() {
            println!("metrics: {} => value: {:?}", key.key().name(), value);
        }

        let database = Arc::get_mut(&mut executor.database).unwrap();
        database.state.merge_transitions(BundleRetention::Reverts);
        (parallel_result, database.state.take_bundle())
    });

    common::compare_execution_result(&reth_result.0.results, &parallel_result.0.results);

    common::compare_bundle_state(&reth_result.1, &parallel_result.1);

    // TODO(gravity_nekomoto): compare the receipts root
}

#[test]
fn mainnet() {
    if let Ok(block_number) = std::env::var("BLOCK_NUMBER").map(|s| s.parse().unwrap()) {
        // Test a specific block
        let bytecodes = common::load_bytecodes_from_disk();
        let (block, accounts, block_hashes) = common::load_block_from_disk(block_number);
        test_execute_alloy(block, InMemoryDB::new(accounts, bytecodes.clone(), block_hashes));
        return;
    }

    common::for_each_block_from_disk(|block, db| {
        println!(
            "Block {}({} txs, {} gas)",
            block.header.number,
            block.transactions.len(),
            block.header.gas_used
        );

        test_execute_alloy(block, db);
    });
}
