pub mod common;

use crate::common::{MINER_ADDRESS, START_ADDRESS};
use common::storage::InMemoryDB;
use metrics_util::debugging::DebugValue;

use revm::primitives::alloy_primitives::U160;
use revm::primitives::{Address, TransactTo, TxEnv, U256};
use std::collections::HashMap;

const GIGA_GAS: u64 = 1_000_000_000;

#[test]
fn native_gigagas() {
    let block_size = (GIGA_GAS as f64 / common::TRANSFER_GAS_LIMIT as f64).ceil() as usize;
    let accounts = common::mock_block_accounts(START_ADDRESS, block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    // START_ADDRESS + block_size
    // let txs: Vec<TxEnv> = (1..=block_size)
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let address = Address::from(U160::from(START_ADDRESS + i));
            TxEnv {
                caller: address,
                transact_to: TransactTo::Call(address),
                value: U256::from(1),
                gas_limit: common::TRANSFER_GAS_LIMIT,
                gas_price: U256::from(1),
                nonce: None,
                ..TxEnv::default()
            }
        })
        .collect();
    common::compare_evm_execute(
        db,
        txs,
        true,
        [
            ("grevm.parallel_round_calls", DebugValue::Counter(1)),
            ("grevm.sequential_execute_calls", DebugValue::Counter(0)),
            ("grevm.parallel_tx_cnt", DebugValue::Counter(block_size as u64)),
            ("grevm.conflict_tx_cnt", DebugValue::Counter(0)),
            ("grevm.unconfirmed_tx_cnt", DebugValue::Counter(0)),
            ("grevm.reusable_tx_cnt", DebugValue::Counter(0)),
            ("grevm.skip_validation_cnt", DebugValue::Counter(block_size as u64)),
            ("grevm.partition_num_tx_diff", DebugValue::Gauge(1.0.into())),
        ]
        .into_iter()
        .collect(),
    );
}

#[test]
fn native_transfers_independent() {
    let block_size = 10_000; // number of transactions
    let accounts = common::mock_block_accounts(START_ADDRESS, block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let address = Address::from(U160::from(START_ADDRESS + i));
            TxEnv {
                caller: address,
                transact_to: TransactTo::Call(address),
                value: U256::from(1),
                gas_limit: common::TRANSFER_GAS_LIMIT,
                gas_price: U256::from(1),
                nonce: Some(1),
                ..TxEnv::default()
            }
        })
        .collect();
    common::compare_evm_execute(
        db,
        txs,
        true,
        [
            ("grevm.parallel_round_calls", DebugValue::Counter(1)),
            ("grevm.sequential_execute_calls", DebugValue::Counter(0)),
            ("grevm.parallel_tx_cnt", DebugValue::Counter(block_size as u64)),
            ("grevm.conflict_tx_cnt", DebugValue::Counter(0)),
            ("grevm.unconfirmed_tx_cnt", DebugValue::Counter(0)),
            ("grevm.reusable_tx_cnt", DebugValue::Counter(0)),
            ("grevm.skip_validation_cnt", DebugValue::Counter(block_size as u64)),
            ("grevm.partition_num_tx_diff", DebugValue::Gauge(1.0.into())),
        ]
        .into_iter()
        .collect(),
    );
}

#[test]
fn native_with_same_sender() {
    let block_size = 100;
    let accounts = common::mock_block_accounts(START_ADDRESS, block_size + 1);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());

    let sender_address = Address::from(U160::from(START_ADDRESS));
    let receiver_address = Address::from(U160::from(START_ADDRESS + 1));
    let mut sender_nonce = 0;
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let (address, to, nonce) = if i % 4 != 1 {
                (
                    Address::from(U160::from(START_ADDRESS + i)),
                    Address::from(U160::from(START_ADDRESS + i)),
                    1,
                )
            } else {
                sender_nonce += 1;
                (sender_address, receiver_address, sender_nonce)
            };

            TxEnv {
                caller: address,
                transact_to: TransactTo::Call(to),
                value: U256::from(i),
                gas_limit: common::TRANSFER_GAS_LIMIT,
                gas_price: U256::from(1),
                // If setting nonce, then nonce validation against the account's nonce,
                // the parallel execution will fail for the nonce validation.
                // However, the failed evm.transact() doesn't generate write set,
                // then there's no dependency can be detected even two txs are related.
                // TODO(gaoxin): lazily update nonce
                nonce: None,
                ..TxEnv::default()
            }
        })
        .collect();
    common::compare_evm_execute(
        db,
        txs,
        false,
        [
            ("grevm.parallel_round_calls", DebugValue::Counter(2)),
            ("grevm.sequential_execute_calls", DebugValue::Counter(0)),
            ("grevm.parallel_tx_cnt", DebugValue::Counter(block_size as u64)),
            ("grevm.conflict_tx_cnt", DebugValue::Counter(24)),
            ("grevm.unconfirmed_tx_cnt", DebugValue::Counter(71)),
            ("grevm.reusable_tx_cnt", DebugValue::Counter(71)),
            ("grevm.partition_num_tx_diff", DebugValue::Gauge(21.0.into())),
        ]
        .into_iter()
        .collect(),
    );
}

#[test]
fn native_with_all_related() {
    let block_size = 100;
    let accounts = common::mock_block_accounts(START_ADDRESS, block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            // tx(i) => tx(i+1), all transactions should execute sequentially.
            let from = Address::from(U160::from(START_ADDRESS + i));
            let to = Address::from(U160::from(START_ADDRESS + i + 1));

            TxEnv {
                caller: from,
                transact_to: TransactTo::Call(to),
                value: U256::from(1000),
                gas_limit: common::TRANSFER_GAS_LIMIT,
                gas_price: U256::from(1),
                nonce: None,
                ..TxEnv::default()
            }
        })
        .collect();
    common::compare_evm_execute(
        db,
        txs,
        false,
        [
            ("grevm.parallel_round_calls", DebugValue::Counter(2)),
            ("grevm.sequential_execute_calls", DebugValue::Counter(0)),
            ("grevm.parallel_tx_cnt", DebugValue::Counter(block_size as u64)),
            ("grevm.conflict_tx_cnt", DebugValue::Counter(96)),
            ("grevm.unconfirmed_tx_cnt", DebugValue::Counter(0)),
            ("grevm.reusable_tx_cnt", DebugValue::Counter(0)),
            ("grevm.concurrent_partition_num", DebugValue::Gauge(1.0.into())), // all transactions are related, so running in one partition
            ("grevm.partition_num_tx_diff", DebugValue::Gauge(0.0.into())),
        ]
        .into_iter()
        .collect(),
    );
}

#[test]
fn native_with_unconfirmed_reuse() {
    let block_size = 100;
    let accounts = common::mock_block_accounts(START_ADDRESS, block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let (from, to) = if i % 10 == 0 {
                (
                    Address::from(U160::from(START_ADDRESS + i)),
                    Address::from(U160::from(START_ADDRESS + i + 1)),
                )
            } else {
                (
                    Address::from(U160::from(START_ADDRESS + i)),
                    Address::from(U160::from(START_ADDRESS + i)),
                )
            };
            // tx0 tx10, tx20, tx30 ... tx90 will produce dependency for the next tx,
            // so tx1, tx11, tx21, tx31, tx91 maybe redo on next round.
            // However, tx2 ~ tx9, tx12 ~ tx19 can reuse the result from the pre-round context.
            TxEnv {
                caller: from,
                transact_to: TransactTo::Call(to),
                value: U256::from(100),
                gas_limit: common::TRANSFER_GAS_LIMIT,
                gas_price: U256::from(1),
                nonce: None,
                ..TxEnv::default()
            }
        })
        .collect();
    common::compare_evm_execute(db, txs, false, HashMap::new());
}

#[test]
fn native_zero_or_one_tx() {
    let accounts = common::mock_block_accounts(START_ADDRESS, 0);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = vec![];
    // empty block
    common::compare_evm_execute(db, txs, false, HashMap::new());

    // one tx
    let txs = vec![TxEnv {
        caller: Address::from(U160::from(START_ADDRESS)),
        transact_to: TransactTo::Call(Address::from(U160::from(START_ADDRESS))),
        value: U256::from(1000),
        gas_limit: common::TRANSFER_GAS_LIMIT,
        gas_price: U256::from(1),
        nonce: None,
        ..TxEnv::default()
    }];
    let accounts = common::mock_block_accounts(START_ADDRESS, 1);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    common::compare_evm_execute(db, txs, false, HashMap::new());
}

#[test]
fn native_loaded_not_existing_account() {
    let block_size = 100; // number of transactions
    let mut accounts = common::mock_block_accounts(START_ADDRESS, block_size);
    // remove miner address
    let miner_address = Address::from(U160::from(MINER_ADDRESS));
    accounts.remove(&miner_address);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (START_ADDRESS..START_ADDRESS + block_size)
        .map(|i| {
            let address = Address::from(U160::from(i));
            // transfer to not existing account
            let to = Address::from(U160::from(i + block_size));
            TxEnv {
                caller: address,
                transact_to: TransactTo::Call(to),
                value: U256::from(999),
                gas_limit: common::TRANSFER_GAS_LIMIT,
                gas_price: U256::from(1),
                nonce: Some(1),
                ..TxEnv::default()
            }
        })
        .collect();
    common::compare_evm_execute(db, txs, true, HashMap::new());
}

#[test]
fn native_transfer_with_beneficiary() {
    let block_size = 20; // number of transactions
    let accounts = common::mock_block_accounts(START_ADDRESS, block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let mut txs: Vec<TxEnv> = (START_ADDRESS..START_ADDRESS + block_size)
        .map(|i| {
            let address = Address::from(U160::from(i));
            TxEnv {
                caller: address,
                transact_to: TransactTo::Call(address),
                value: U256::from(100),
                gas_limit: common::TRANSFER_GAS_LIMIT,
                gas_price: U256::from(1),
                nonce: None,
                ..TxEnv::default()
            }
        })
        .collect();
    let start_address = Address::from(U160::from(START_ADDRESS));
    let miner_address = Address::from(U160::from(MINER_ADDRESS));
    // miner => start
    txs.push(TxEnv {
        caller: miner_address,
        transact_to: TransactTo::Call(start_address),
        value: U256::from(1),
        gas_limit: common::TRANSFER_GAS_LIMIT,
        gas_price: U256::from(1),
        nonce: Some(1),
        ..TxEnv::default()
    });
    // miner => start
    txs.push(TxEnv {
        caller: miner_address,
        transact_to: TransactTo::Call(start_address),
        value: U256::from(1),
        gas_limit: common::TRANSFER_GAS_LIMIT,
        gas_price: U256::from(1),
        nonce: Some(2),
        ..TxEnv::default()
    });
    // start => miner
    txs.push(TxEnv {
        caller: start_address,
        transact_to: TransactTo::Call(miner_address),
        value: U256::from(1),
        gas_limit: common::TRANSFER_GAS_LIMIT,
        gas_price: U256::from(1),
        nonce: Some(2),
        ..TxEnv::default()
    });
    // miner => miner
    txs.push(TxEnv {
        caller: miner_address,
        transact_to: TransactTo::Call(miner_address),
        value: U256::from(1),
        gas_limit: common::TRANSFER_GAS_LIMIT,
        gas_price: U256::from(1),
        nonce: Some(3),
        ..TxEnv::default()
    });
    common::compare_evm_execute(
        db,
        txs,
        true,
        [
            ("grevm.parallel_round_calls", DebugValue::Counter(2)),
            ("grevm.sequential_execute_calls", DebugValue::Counter(0)),
            ("grevm.parallel_tx_cnt", DebugValue::Counter(24 as u64)),
            ("grevm.conflict_tx_cnt", DebugValue::Counter(4)),
            ("grevm.unconfirmed_tx_cnt", DebugValue::Counter(0)),
            ("grevm.reusable_tx_cnt", DebugValue::Counter(0)),
        ]
        .into_iter()
        .collect(),
    );
}
