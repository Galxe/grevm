#![allow(missing_docs)]

use grevm::test_utils::{
    TRANSFER_GAS_LIMIT,
    common::{account, execute, storage::InMemoryDB},
};
use revm_context::TxEnv;
use revm_primitives::{HashMap, TxKind, U256};

const GIGA_GAS: u64 = 1_000_000_000;

#[test]
fn native_gigagas() {
    let block_size = (GIGA_GAS as f64 / TRANSFER_GAS_LIMIT as f64).ceil() as usize;
    let accounts = account::mock_block_accounts(block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let address = account::mock_eoa_address(i);
            TxEnv {
                caller: address,
                kind: TxKind::Call(address),
                value: U256::from(1),
                gas_limit: TRANSFER_GAS_LIMIT,
                gas_price: 1,
                nonce: 1,
                ..TxEnv::default()
            }
        })
        .collect();
    execute::compare_evm_execute(db, txs, true, false, Default::default());
}

#[test]
fn native_transfers_independent() {
    let block_size = 10_000; // number of transactions
    let accounts = account::mock_block_accounts(block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let address = account::mock_eoa_address(i);
            TxEnv {
                caller: address,
                kind: TxKind::Call(address),
                value: U256::from(1),
                gas_limit: TRANSFER_GAS_LIMIT,
                gas_price: 1,
                nonce: 1,
                ..TxEnv::default()
            }
        })
        .collect();
    execute::compare_evm_execute(db, txs, true, false, Default::default());
}

#[test]
fn native_with_same_sender() {
    let block_size = 100;
    let accounts = account::mock_block_accounts(block_size + 1);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());

    let sender_address = account::mock_eoa_address(0);
    let receiver_address = account::mock_eoa_address(1);
    let mut sender_nonce = 0;
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let (address, to, _) = if i % 4 != 1 {
                (account::mock_eoa_address(i), account::mock_eoa_address(i), 1)
            } else {
                sender_nonce += 1;
                (sender_address, receiver_address, sender_nonce)
            };

            TxEnv {
                caller: address,
                kind: TxKind::Call(to),
                value: U256::from(i),
                gas_limit: TRANSFER_GAS_LIMIT,
                gas_price: 1,
                // If setting nonce, then nonce validation against the account's nonce,
                // the parallel execution will fail for the nonce validation.
                // However, the failed evm.transact() doesn't generate write set,
                // then there's no dependency can be detected even two txs are related.
                // TODO(gaoxin): lazily update nonce
                nonce: 0,
                ..TxEnv::default()
            }
        })
        .collect();
    execute::compare_evm_execute(db, txs, false, true, Default::default());
}

#[test]
fn native_with_all_related() {
    let block_size = 47620;
    let accounts = account::mock_block_accounts(block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            // tx(i) => tx(i+1), all transactions should execute sequentially.
            let from = account::mock_eoa_address(i);
            let to = account::mock_eoa_address(i + 1);

            TxEnv {
                caller: from,
                kind: TxKind::Call(to),
                value: U256::from(1000),
                gas_limit: TRANSFER_GAS_LIMIT,
                gas_price: 1,
                nonce: 0,
                ..TxEnv::default()
            }
        })
        .collect();
    execute::compare_evm_execute(db, txs, false, true, Default::default());
}

#[test]
fn native_with_unconfirmed_reuse() {
    let block_size = 100;
    let accounts = account::mock_block_accounts(block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let (from, to) = if i % 10 == 0 {
                (account::mock_eoa_address(i), account::mock_eoa_address(i + 1))
            } else {
                (account::mock_eoa_address(i), account::mock_eoa_address(i))
            };
            // tx0 tx10, tx20, tx30 ... tx90 will produce dependency for the next tx,
            // so tx1, tx11, tx21, tx31, tx91 maybe redo on next round.
            // However, tx2 ~ tx9, tx12 ~ tx19 can reuse the result from the pre-round context.
            TxEnv {
                caller: from,
                kind: TxKind::Call(to),
                value: U256::from(100),
                gas_limit: TRANSFER_GAS_LIMIT,
                gas_price: 1,
                nonce: 0,
                ..TxEnv::default()
            }
        })
        .collect();
    execute::compare_evm_execute(db, txs, false, true, HashMap::new());
}

#[test]
fn native_zero_or_one_tx() {
    let accounts = account::mock_block_accounts(0);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = vec![];
    // empty block
    execute::compare_evm_execute(db, txs, false, false, HashMap::new());

    // one tx
    let txs = vec![TxEnv {
        caller: account::mock_eoa_address(0),
        kind: TxKind::Call(account::mock_eoa_address(0)),
        value: U256::from(1000),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    }];
    let accounts = account::mock_block_accounts(1);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    execute::compare_evm_execute(db, txs, false, false, HashMap::new());
}

#[test]
fn native_loaded_not_existing_account() {
    let block_size = 100; // number of transactions
    let mut accounts = account::mock_block_accounts(block_size);
    // remove miner address
    accounts.remove(&account::MINER_ADDRESS);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let address = account::mock_eoa_address(i);
            // transfer to not existing account
            let to = account::mock_eoa_address(i + block_size);
            TxEnv {
                caller: address,
                kind: TxKind::Call(to),
                value: U256::from(999),
                gas_limit: TRANSFER_GAS_LIMIT,
                gas_price: 1,
                nonce: 1,
                ..TxEnv::default()
            }
        })
        .collect();
    execute::compare_evm_execute(db, txs, true, false, HashMap::new());
}

#[test]
fn native_transfer_with_beneficiary() {
    let block_size = 20; // number of transactions
    let accounts = account::mock_block_accounts(block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let mut txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let address = account::mock_eoa_address(i);
            TxEnv {
                caller: address,
                kind: TxKind::Call(address),
                value: U256::from(100),
                gas_limit: TRANSFER_GAS_LIMIT,
                gas_price: 1,
                nonce: 1,
                ..TxEnv::default()
            }
        })
        .collect();
    let start_address = account::mock_eoa_address(0);
    // miner => start
    txs.push(TxEnv {
        caller: account::MINER_ADDRESS,
        kind: TxKind::Call(start_address),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    });
    // miner => start
    txs.push(TxEnv {
        caller: account::MINER_ADDRESS,
        kind: TxKind::Call(start_address),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 2,
        ..TxEnv::default()
    });
    // start => miner
    txs.push(TxEnv {
        caller: start_address,
        kind: TxKind::Call(account::MINER_ADDRESS),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 2,
        ..TxEnv::default()
    });
    // miner => miner
    txs.push(TxEnv {
        caller: account::MINER_ADDRESS,
        kind: TxKind::Call(account::MINER_ADDRESS),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 3,
        ..TxEnv::default()
    });
    execute::compare_evm_execute(db, txs, true, false, Default::default());
}

#[test]
fn native_transfer_with_beneficiary_enough() {
    let block_size = 20; // number of transactions
    let accounts = account::mock_block_accounts(block_size);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let mut txs: Vec<TxEnv> = (0..block_size)
        .map(|i| {
            let address = account::mock_eoa_address(i);
            TxEnv {
                caller: address,
                kind: TxKind::Call(address),
                value: U256::from(100),
                gas_limit: TRANSFER_GAS_LIMIT,
                gas_price: 1,
                nonce: 1,
                ..TxEnv::default()
            }
        })
        .collect();
    let start_address = account::mock_eoa_address(0);
    // start => miner
    txs.push(TxEnv {
        caller: start_address,
        kind: TxKind::Call(account::MINER_ADDRESS),
        value: U256::from(100000),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 2,
        ..TxEnv::default()
    });
    // miner => start
    txs.push(TxEnv {
        caller: account::MINER_ADDRESS,
        kind: TxKind::Call(start_address),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    });
    // miner => start
    txs.push(TxEnv {
        caller: account::MINER_ADDRESS,
        kind: TxKind::Call(start_address),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 2,
        ..TxEnv::default()
    });
    // miner => miner
    txs.push(TxEnv {
        caller: account::MINER_ADDRESS,
        kind: TxKind::Call(account::MINER_ADDRESS),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 3,
        ..TxEnv::default()
    });
    execute::compare_evm_execute(db, txs, true, false, Default::default());
}
