#![allow(missing_docs)]

use grevm::{
    InvalidTransaction, ParallelState, Scheduler, TxExecutionOutcome,
    test_utils::{
        TRANSFER_GAS_LIMIT,
        common::{account, execute, storage::InMemoryDB},
    },
};
use revm_context::{BlockEnv, CfgEnv, TxEnv};
use revm_primitives::{HashMap, TxKind, U256, hardfork::SpecId};
use revm_state::Bytecode;
use std::sync::Arc;

const GIGA_GAS: u64 = 1_000_000_000;
const MIN_PARALLEL_BLOCK_SIZE: usize = 64;

fn execute_outcomes(txs: Vec<TxEnv>) -> Vec<TxExecutionOutcome> {
    let accounts = account::mock_block_accounts(MIN_PARALLEL_BLOCK_SIZE + 1);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    execute_outcomes_with_db(db, txs)
}

fn execute_outcomes_with_db(db: InMemoryDB, txs: Vec<TxEnv>) -> Vec<TxExecutionOutcome> {
    execute::compare_evm_execute_skipping_invalid_with_spec(db, txs, SpecId::SHANGHAI)
}

fn independent_transfer(index: usize) -> TxEnv {
    let sender = account::mock_eoa_address(index);
    TxEnv {
        caller: sender,
        kind: TxKind::Call(sender),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 1,
        ..TxEnv::default()
    }
}

#[test]
fn nonce_error_falls_back_and_skips_without_incrementing_nonce() {
    let sender = account::mock_eoa_address(0);
    let mut txs = vec![
        independent_transfer(0),
        TxEnv {
            caller: sender,
            kind: TxKind::Call(sender),
            value: U256::from(1),
            gas_limit: TRANSFER_GAS_LIMIT,
            gas_price: 1,
            nonce: 1,
            ..TxEnv::default()
        },
        TxEnv {
            caller: sender,
            kind: TxKind::Call(sender),
            value: U256::from(1),
            gas_limit: TRANSFER_GAS_LIMIT,
            gas_price: 1,
            nonce: 2,
            ..TxEnv::default()
        },
    ];
    txs.extend((3..MIN_PARALLEL_BLOCK_SIZE).map(independent_transfer));

    let outcomes = execute_outcomes(txs);
    assert_eq!(outcomes.len(), MIN_PARALLEL_BLOCK_SIZE);
    assert!(matches!(outcomes[0], TxExecutionOutcome::Executed(_)));
    assert!(matches!(
        outcomes[1],
        TxExecutionOutcome::Skipped(InvalidTransaction::NonceTooLow { .. })
    ));
    assert!(matches!(outcomes[2], TxExecutionOutcome::Executed(_)));
}

#[test]
fn consecutive_nonce_errors_are_revalidated_and_suffix_executes() {
    let sender = account::mock_eoa_address(0);
    let sender_tx = |nonce| TxEnv {
        caller: sender,
        kind: TxKind::Call(sender),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce,
        ..TxEnv::default()
    };
    let mut txs = vec![sender_tx(3), sender_tx(0), sender_tx(1)];
    txs.extend((3..MIN_PARALLEL_BLOCK_SIZE).map(independent_transfer));

    let outcomes = execute_outcomes(txs);
    assert_eq!(outcomes.len(), MIN_PARALLEL_BLOCK_SIZE);
    assert!(matches!(
        outcomes[0],
        TxExecutionOutcome::Skipped(InvalidTransaction::NonceTooHigh { .. })
    ));
    assert!(matches!(
        outcomes[1],
        TxExecutionOutcome::Skipped(InvalidTransaction::NonceTooLow { .. })
    ));
    assert!(matches!(outcomes[2], TxExecutionOutcome::Executed(_)));
}

#[test]
fn sender_with_code_is_skipped_and_suffix_executes() {
    let sender = account::mock_eoa_address(0);
    let mut accounts = account::mock_block_accounts(MIN_PARALLEL_BLOCK_SIZE + 1);
    let code = Bytecode::new_raw(vec![0x00].into());
    let code_hash = code.hash_slow();
    let sender_account = accounts.get_mut(&sender).expect("sender account must exist");
    sender_account.info.code_hash = code_hash;
    sender_account.info.code = Some(code.clone());
    let mut bytecodes = HashMap::default();
    bytecodes.insert(code_hash, code);
    let db = InMemoryDB::new(accounts, bytecodes, Default::default());

    let mut txs = vec![independent_transfer(0), independent_transfer(1)];
    txs.extend((2..MIN_PARALLEL_BLOCK_SIZE).map(independent_transfer));

    let outcomes = execute_outcomes_with_db(db, txs);
    assert_eq!(outcomes.len(), MIN_PARALLEL_BLOCK_SIZE);
    assert_eq!(outcomes[0], TxExecutionOutcome::Skipped(InvalidTransaction::RejectCallerWithCode));
    assert!(matches!(outcomes[1], TxExecutionOutcome::Executed(_)));
}

#[test]
fn intrinsic_gas_error_falls_back_and_continues_suffix() {
    let accounts = account::mock_block_accounts(MIN_PARALLEL_BLOCK_SIZE + 1);
    let db = Arc::new(InMemoryDB::new(accounts, Default::default(), Default::default()));
    let mut invalid = independent_transfer(0);
    invalid.gas_limit = 1;
    let mut txs = vec![invalid];
    txs.extend((1..MIN_PARALLEL_BLOCK_SIZE).map(independent_transfer));

    let state = ParallelState::new(db, true, false);
    let scheduler = Scheduler::new(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv::default(),
        Arc::new(txs),
        state,
        None,
    );
    scheduler.parallel_execute(Some(23)).expect("transaction validation errors must be skipped");
    let (outcomes, _) = scheduler.take_result_and_state();

    assert_eq!(outcomes.len(), MIN_PARALLEL_BLOCK_SIZE);
    assert!(matches!(
        outcomes[0],
        TxExecutionOutcome::Skipped(InvalidTransaction::CallGasCostMoreThanGasLimit { .. })
    ));
    assert!(matches!(outcomes[1], TxExecutionOutcome::Executed(_)));
}

#[test]
fn basefee_error_falls_back_and_continues_suffix() {
    let accounts = account::mock_block_accounts(MIN_PARALLEL_BLOCK_SIZE + 1);
    let db = Arc::new(InMemoryDB::new(accounts, Default::default(), Default::default()));
    let mut txs: Vec<_> = (0..MIN_PARALLEL_BLOCK_SIZE).map(independent_transfer).collect();
    for tx in &mut txs {
        tx.gas_price = 100;
    }
    txs[0].gas_price = 50;

    let state = ParallelState::new(db, true, false);
    let env = BlockEnv { basefee: 100, ..Default::default() };
    let scheduler =
        Scheduler::new(CfgEnv::new_with_spec(SpecId::SHANGHAI), env, Arc::new(txs), state, None);
    scheduler.parallel_execute(Some(23)).expect("basefee-invalid transaction must be skipped");
    let (outcomes, _) = scheduler.take_result_and_state();

    assert_eq!(outcomes.len(), MIN_PARALLEL_BLOCK_SIZE);
    assert_eq!(
        outcomes[0],
        TxExecutionOutcome::Skipped(InvalidTransaction::GasPriceLessThanBasefee)
    );
    assert!(matches!(outcomes[1], TxExecutionOutcome::Executed(_)));
}

#[test]
fn nonce_overflow_from_parallel_commit_falls_back_and_continues_suffix() {
    let sender = account::mock_eoa_address(0);
    let mut accounts = account::mock_block_accounts(MIN_PARALLEL_BLOCK_SIZE + 1);
    accounts.get_mut(&sender).expect("sender account must exist").info.nonce = u64::MAX;
    let db = Arc::new(InMemoryDB::new(accounts, Default::default(), Default::default()));
    let mut txs: Vec<_> = (0..MIN_PARALLEL_BLOCK_SIZE).map(independent_transfer).collect();
    txs[0].nonce = u64::MAX;

    let state = ParallelState::new(db, true, false);
    let scheduler = Scheduler::new(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv::default(),
        Arc::new(txs),
        state,
        None,
    );
    scheduler.parallel_execute(Some(23)).expect("nonce overflow must be skipped");
    let (outcomes, _) = scheduler.take_result_and_state();

    assert_eq!(outcomes.len(), MIN_PARALLEL_BLOCK_SIZE);
    assert_eq!(
        outcomes[0],
        TxExecutionOutcome::Skipped(InvalidTransaction::NonceOverflowInTransaction)
    );
    assert!(matches!(outcomes[1], TxExecutionOutcome::Executed(_)));
}

#[test]
fn insufficient_funds_falls_back_and_continues_suffix() {
    let sender = account::mock_eoa_address(1);
    let mut txs = vec![
        independent_transfer(0),
        TxEnv {
            caller: sender,
            kind: TxKind::Call(sender),
            value: U256::from(1_000_000_000_000_000_000u128),
            gas_limit: TRANSFER_GAS_LIMIT,
            gas_price: 1,
            nonce: 1,
            ..TxEnv::default()
        },
        independent_transfer(2),
    ];
    txs.extend((3..MIN_PARALLEL_BLOCK_SIZE).map(independent_transfer));

    let outcomes = execute_outcomes(txs);
    assert_eq!(outcomes.len(), MIN_PARALLEL_BLOCK_SIZE);
    assert!(matches!(
        outcomes[1],
        TxExecutionOutcome::Skipped(InvalidTransaction::LackOfFundForMaxFee { .. })
    ));
    assert!(matches!(outcomes[2], TxExecutionOutcome::Executed(_)));
}

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
    execute::compare_evm_execute(db, txs, false, Default::default());
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
    execute::compare_evm_execute(db, txs, false, Default::default());
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
    execute::compare_evm_execute(db, txs, true, Default::default());
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
    execute::compare_evm_execute(db, txs, true, Default::default());
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
    execute::compare_evm_execute(db, txs, true, HashMap::default());
}

#[test]
fn native_zero_or_one_tx() {
    let accounts = account::mock_block_accounts(0);
    let db = InMemoryDB::new(accounts, Default::default(), Default::default());
    let txs: Vec<TxEnv> = vec![];
    // empty block
    execute::compare_evm_execute(db, txs, false, HashMap::default());

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
    execute::compare_evm_execute(db, txs, false, HashMap::default());
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
    execute::compare_evm_execute(db, txs, false, HashMap::default());
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
    execute::compare_evm_execute(db, txs, false, Default::default());
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
    execute::compare_evm_execute(db, txs, false, Default::default());
}
