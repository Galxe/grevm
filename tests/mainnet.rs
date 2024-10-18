mod common;
use std::sync::Arc;

use alloy_chains::NamedChain;
use alloy_rpc_types::{Block, BlockTransactions};
use common::{compat, storage::InMemoryDB};
use grevm::GrevmScheduler;
use revm::primitives::{Env, TxEnv};

fn test_execute_alloy(block: Block, db: InMemoryDB) {
    let spec_id = compat::get_block_spec(&block.header);
    let block_env = compat::get_block_env(&block.header);
    let txs: Vec<TxEnv> = match block.transactions {
        BlockTransactions::Full(txs) => txs.into_iter().map(|tx| compat::get_tx_env(tx)).collect(),
        _ => panic!("Missing transaction data"),
    };

    let mut env = Env::default();
    env.cfg.chain_id = NamedChain::Mainnet.into();
    env.block = block_env;

    let db = Arc::new(db);

    let reth_result =
        common::execute_revm_sequential(db.clone(), spec_id, env.clone(), txs.clone());

    let executor = GrevmScheduler::new(spec_id, env, db, txs);
    let parallel_result = executor.parallel_execute();

    common::compare_execution_result(
        &reth_result.as_ref().unwrap().results,
        &parallel_result.as_ref().unwrap().results,
    );

    common::compare_bundle_state(
        &reth_result.as_ref().unwrap().state,
        &parallel_result.as_ref().unwrap().state,
    );

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
