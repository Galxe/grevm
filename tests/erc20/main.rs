// Each cluster has one ERC20 contract and X families.
// Each family has Y people.
// Each person performs Z transfers to random people within the family.

use crate::{
    common::START_ADDRESS,
    erc20::{
        GAS_LIMIT, TransactionModeType, TxnBatchConfig, erc20_contract::ERC20Token,
        generate_cluster, generate_cluster_and_txs,
    },
};
use common::storage::InMemoryDB;
use revm::primitives::{Address, U256, alloy_primitives::U160, uint};
use revm_context::TxEnv;
use revm_primitives::TxKind;
use std::collections::HashMap;

#[path = "../common/mod.rs"]
pub mod common;

#[path = "./mod.rs"]
pub mod erc20;

const GIGA_GAS: u64 = 1_000_000_000;

#[test]
fn erc20_gigagas() {
    const PEVM_GAS_LIMIT: u64 = 26_938;
    let block_size = (GIGA_GAS as f64 / PEVM_GAS_LIMIT as f64).ceil() as usize;
    let (mut state, bytecodes, eoa, sca) = generate_cluster(block_size, 1);
    let miner = common::mock_miner_account();
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
    common::compare_evm_execute(
        db,
        txs,
        true,
        false,
        [
            ("grevm.parallel_round_calls", 1),
            ("grevm.sequential_execute_calls", 0),
            ("grevm.parallel_tx_cnt", block_size),
            ("grevm.conflict_tx_cnt", 0),
            ("grevm.skip_validation_cnt", block_size),
        ]
        .into_iter()
        .collect(),
    );
}

#[test]
fn erc20_hints_test() {
    let account1 = Address::from(U160::from(START_ADDRESS + 1));
    let account2 = Address::from(U160::from(START_ADDRESS + 2));
    let account3 = Address::from(U160::from(START_ADDRESS + 3));
    let account4 = Address::from(U160::from(START_ADDRESS + 4));
    let mut accounts = common::mock_block_accounts(START_ADDRESS + 1, 4);
    let mut bytecodes = HashMap::new();
    // START_ADDRESS as contract address
    let contract_address = Address::from(U160::from(START_ADDRESS));
    let galxe_account =
        ERC20Token::new("Galxe Token", "G", 18, 222_222_000_000_000_000_000_000u128)
            .add_balances(
                &[account1, account2, account3, account4],
                uint!(1_000_000_000_000_000_000_U256),
            )
            .add_allowances(&[account1], account2, uint!(50_000_000_000_000_000_U256))
            .build();
    bytecodes.insert(galxe_account.info.code_hash, galxe_account.info.code.clone().unwrap());
    accounts.insert(contract_address, galxe_account);
    // tx0: account1 --(erc20)--> account4
    // tx1: account2 --(erc20)--> account4
    // tx2: account3 --(raw)--> account4
    // so, (tx0, tx1) are independent with (tx2)
    let mut txs: Vec<TxEnv> = vec![
        TxEnv {
            caller: account1,
            kind: TxKind::Call(contract_address),
            value: U256::from(0),
            gas_limit: GAS_LIMIT,
            gas_price: 1,
            nonce: 1,
            ..TxEnv::default()
        },
        TxEnv {
            caller: account2,
            kind: TxKind::Call(contract_address),
            value: U256::from(0),
            gas_limit: GAS_LIMIT,
            gas_price: 1,
            nonce: 1,
            ..TxEnv::default()
        },
        TxEnv {
            caller: account3,
            kind: TxKind::Call(account4),
            value: U256::from(100),
            gas_limit: GAS_LIMIT,
            gas_price: 1,
            nonce: 1,
            ..TxEnv::default()
        },
    ];
    let call_data = ERC20Token::transfer(account4, U256::from(900));
    txs[0].data = call_data.clone();
    txs[1].data = call_data.clone();
    let db = InMemoryDB::new(accounts, bytecodes, Default::default());
    common::compare_evm_execute(db, txs, true, false, Default::default());
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
        erc20::TransactionCallDataType::Transfer,
        TransactionModeType::SameCaller,
    );
    let (mut state, bytecodes, txs) = generate_cluster_and_txs(&batch_txn_config);
    let miner = common::mock_miner_account();
    state.insert(miner.0, miner.1);
    let db = InMemoryDB::new(state, bytecodes, Default::default());
    common::compare_evm_execute(db, txs, true, false, Default::default());
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
        erc20::TransactionCallDataType::Transfer,
        TransactionModeType::Random,
    );

    let mut final_state = HashMap::from([common::mock_miner_account()]);
    let mut final_bytecodes = HashMap::default();
    let mut final_txs = Vec::<TxEnv>::new();
    for _ in 0..1 {
        let (state, bytecodes, txs) = generate_cluster_and_txs(&batch_txn_config);
        final_state.extend(state);
        final_bytecodes.extend(bytecodes);
        final_txs.extend(txs);
    }

    let db = InMemoryDB::new(final_state, final_bytecodes, Default::default());
    common::compare_evm_execute(db, final_txs, true, false, Default::default());
}
