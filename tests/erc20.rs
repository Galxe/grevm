#![allow(missing_docs)]

// Each cluster has one ERC20 contract and X families.
// Each family has Y people.
// Each person performs Z transfers to random people within the family.

use grevm::test_utils::{
    common::{account, execute, storage::InMemoryDB},
    erc20::{
        GAS_LIMIT, TransactionCallDataType, TransactionModeType, TxnBatchConfig,
        erc20_contract::ERC20Token, generate_cluster, generate_cluster_and_txs,
    },
};
use revm::primitives::U256;
use revm_context::TxEnv;
use revm_primitives::{HashMap, TxKind};

const GIGA_GAS: u64 = 1_000_000_000;

#[test]
fn erc20_gigagas() {
    const PEVM_GAS_LIMIT: u64 = 26_938;
    let block_size = (GIGA_GAS as f64 / PEVM_GAS_LIMIT as f64).ceil() as usize;
    let (mut state, bytecodes, eoa, sca) = generate_cluster(block_size, 1);
    let miner = account::mock_miner_account();
    state.insert(miner.0, miner.1);
    let mut txs = Vec::with_capacity(block_size);
    let sca = sca[0];
    for addr in eoa {
        let tx = TxEnv {
            caller: addr,
            kind: TxKind::Call(sca),
            value: U256::from(0),
            gas_limit: GAS_LIMIT,
            gas_price: 1,
            nonce: 0,
            data: ERC20Token::transfer(addr, U256::from(900)),
            ..TxEnv::default()
        };
        txs.push(tx);
    }
    let db = InMemoryDB::new(state, bytecodes, Default::default());
    execute::compare_evm_execute(
        db,
        txs,
        false,
        [
            ("grevm.total_tx_cnt", block_size),
            ("grevm.execution_cnt", block_size),
            ("grevm.conflict_cnt", 0),
            ("grevm.no_dependency_txs", block_size),
            ("grevm.conflict_txs", 0),
        ]
        .into_iter()
        .collect(),
    );
}

#[test]
fn erc20_independent() {
    const NUM_SCA: usize = 1;
    const NUM_EOA: usize = 100;
    const NUM_TXNS_PER_ADDRESS: usize = 1;
    let batch_txn_config = TxnBatchConfig::new(
        NUM_EOA,
        NUM_SCA,
        NUM_TXNS_PER_ADDRESS,
        TransactionCallDataType::Transfer,
        TransactionModeType::SameCaller,
    );
    let (mut state, bytecodes, txs) = generate_cluster_and_txs(&batch_txn_config);
    let miner = account::mock_miner_account();
    state.insert(miner.0, miner.1);
    let db = InMemoryDB::new(state, bytecodes, Default::default());
    execute::compare_evm_execute(db, txs, false, Default::default());
}

#[test]
fn erc20_batch_transfer() {
    const NUM_SCA: usize = 3;
    const NUM_EOA: usize = 10;
    const NUM_TXNS_PER_ADDRESS: usize = 20;

    let batch_txn_config = TxnBatchConfig::new(
        NUM_EOA,
        NUM_SCA,
        NUM_TXNS_PER_ADDRESS,
        TransactionCallDataType::Transfer,
        TransactionModeType::Random,
    );

    let mut final_state = HashMap::from_iter([account::mock_miner_account()]);
    let mut final_bytecodes = HashMap::default();
    let mut final_txs = Vec::<TxEnv>::new();
    for _ in 0..1 {
        let (state, bytecodes, txs) = generate_cluster_and_txs(&batch_txn_config);
        final_state.extend(state);
        final_bytecodes.extend(bytecodes);
        final_txs.extend(txs);
    }

    let db = InMemoryDB::new(final_state, final_bytecodes, Default::default());
    execute::compare_evm_execute(db, final_txs, false, Default::default());
}
