#![allow(missing_docs)]

//! End-to-end tests for grevm's opt-in EIP-7702 delegated-account safety policy.

use grevm::{
    DelegatedSafetyConfig, GrevmConfig, ParallelState, ParallelTakeBundle, Scheduler, SkipReason,
    TxExecutionOutcome,
    test_utils::{
        TRANSFER_GAS_LIMIT,
        common::{account, execute, storage::InMemoryDB},
    },
};
use revm_context::{
    BlockEnv, CfgEnv, TxEnv,
    either::Either,
    result::ExecutionResult,
    transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
};
use revm_database::{PlainAccount, states::bundle_state::BundleRetention};
use revm_primitives::{
    Address, HashMap, KECCAK_EMPTY, TxKind, U256, alloy_primitives::U160, hardfork::SpecId,
};
use revm_state::{AccountInfo, Bytecode};
use std::sync::Arc;

const BLOCK_SIZE: usize = 100;
const ONE_ETHER: u128 = 1_000_000_000_000_000_000;
const RESERVE: u128 = 1_000_000_000_000_000;

fn authority() -> Address {
    Address::from(U160::from(900_000))
}

fn receiver() -> Address {
    Address::from(U160::from(900_001))
}

fn delegate_target() -> Address {
    Address::from(U160::from(910_000))
}

fn contract_target() -> Address {
    Address::from(U160::from(910_001))
}

fn eoa_account(balance: u128, nonce: u64) -> PlainAccount {
    PlainAccount {
        info: AccountInfo {
            balance: U256::from(balance),
            nonce,
            code_hash: KECCAK_EMPTY,
            code: None,
        },
        storage: Default::default(),
    }
}

fn contract_account(code: &Bytecode) -> PlainAccount {
    PlainAccount {
        info: AccountInfo {
            nonce: 1,
            code_hash: code.hash_slow(),
            code: Some(code.clone()),
            ..Default::default()
        },
        storage: Default::default(),
    }
}

fn database(target_code: Bytecode) -> InMemoryDB {
    let mut accounts = account::mock_block_accounts(BLOCK_SIZE);
    accounts.insert(authority(), eoa_account(ONE_ETHER, 0));
    accounts.insert(receiver(), eoa_account(ONE_ETHER, 0));
    accounts.insert(delegate_target(), contract_account(&target_code));
    let mut bytecodes = HashMap::default();
    bytecodes.insert(target_code.hash_slow(), target_code);
    InMemoryDB::new(accounts, bytecodes, Default::default())
}

fn insert_contract(db: &mut InMemoryDB, address: Address, code: Bytecode) {
    db.accounts.insert(address, contract_account(&code));
    db.bytecodes.insert(code.hash_slow(), code);
}

fn padding_tx(index: usize) -> TxEnv {
    let caller = account::mock_eoa_address(index);
    TxEnv {
        caller,
        kind: TxKind::Call(caller),
        value: U256::from(1),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce: 1,
        ..Default::default()
    }
}

fn delegated_tx(index: usize, code_address: Address) -> TxEnv {
    let authorization = Authorization { chain_id: U256::ZERO, address: code_address, nonce: 0 };
    TxEnv {
        tx_type: 4,
        caller: account::mock_eoa_address(index),
        kind: TxKind::Call(authority()),
        gas_limit: 400_000,
        gas_price: 1,
        nonce: 1,
        authorization_list: vec![Either::Right(RecoveredAuthorization::new_unchecked(
            authorization,
            RecoveredAuthority::Valid(authority()),
        ))],
        ..Default::default()
    }
}

fn authority_tx(nonce: u64) -> TxEnv {
    TxEnv {
        caller: authority(),
        kind: TxKind::Call(receiver()),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce,
        ..Default::default()
    }
}

fn call_code(target: Address, value_opcode: &[u8]) -> Bytecode {
    let mut code = vec![
        0x60, 0x00, // return size
        0x60, 0x00, // return offset
        0x60, 0x00, // args size
        0x60, 0x00, // args offset
    ];
    code.extend_from_slice(value_opcode);
    code.push(0x73); // PUSH20
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[0x62, 0x0f, 0x42, 0x40, 0xf1, 0x50, 0x00]);
    Bytecode::new_raw(code.into())
}

fn drain_code(target: Address) -> Bytecode {
    call_code(target, &[0x47]) // SELFBALANCE
}

fn leave_reserve_code(target: Address) -> Bytecode {
    let mut value = vec![0x7f]; // PUSH32 reserve; SELFBALANCE; SUB
    value.extend_from_slice(&U256::from(RESERVE).to_be_bytes::<32>());
    value.extend_from_slice(&[0x47, 0x03]);
    call_code(target, &value)
}

fn zero_value_call_code(target: Address) -> Bytecode {
    call_code(target, &[0x60, 0x00])
}

fn selfdestruct_code(target: Address) -> Bytecode {
    let mut code = vec![0x73];
    code.extend_from_slice(target.as_slice());
    code.push(0xff);
    Bytecode::new_raw(code.into())
}

fn execute_block(
    db: InMemoryDB,
    txs: Vec<TxEnv>,
    safety: DelegatedSafetyConfig,
    force_sequential: bool,
) -> (Vec<TxExecutionOutcome>, revm_database::BundleState) {
    let txs = Arc::new(txs);
    let mut block = BlockEnv::default();
    block.beneficiary = account::MINER_ADDRESS;
    let state = ParallelState::new(Arc::new(db), true, true);
    let config = GrevmConfig {
        concurrency_level: 23,
        force_sequential,
        min_parallel_txs: 0,
        delegated_safety: safety,
    };
    let scheduler = Scheduler::new_with_config(
        CfgEnv::new_with_spec(SpecId::PRAGUE),
        block,
        txs,
        state,
        false,
        None,
        config,
    );
    scheduler.execute().expect("block execution failed");
    let (outcomes, mut state) = scheduler.take_result_and_state();
    let bundle = state.parallel_take_bundle(BundleRetention::Reverts);
    (outcomes, bundle)
}

fn execute_safety(
    db: InMemoryDB,
    txs: Vec<TxEnv>,
    force_sequential: bool,
) -> (Vec<TxExecutionOutcome>, revm_database::BundleState) {
    execute_block(db, txs, DelegatedSafetyConfig::enabled(U256::from(RESERVE)), force_sequential)
}

fn final_info<'a>(
    db: &'a InMemoryDB,
    bundle: &'a revm_database::BundleState,
    address: Address,
) -> &'a AccountInfo {
    bundle
        .state
        .get(&address)
        .and_then(|account| account.info.as_ref())
        .unwrap_or_else(|| &db.accounts.get(&address).unwrap().info)
}

#[test]
fn delegated_create_and_create2_are_blocked_without_extra_nonce() {
    for opcode in [0xf0, 0xf5] {
        let create_code = if opcode == 0xf0 {
            vec![0x60, 0, 0x60, 0, 0x60, 0, opcode, 0x50, 0x00]
        } else {
            vec![0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, opcode, 0x50, 0x00]
        };
        let db = database(Bytecode::new_raw(create_code.into()));
        let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
        txs[10] = delegated_tx(10, delegate_target());
        txs[11] = authority_tx(1);
        txs[12] = authority_tx(2);

        let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
        assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Halt { .. })));
        assert!(matches!(outcomes[11], TxExecutionOutcome::Executed(_)));
        assert!(matches!(outcomes[12], TxExecutionOutcome::Executed(_)));
        assert_eq!(final_info(&db, &bundle, authority()).nonce, 3);
    }
}

#[test]
fn disabling_policy_preserves_upstream_delegated_create_semantics() {
    let code = Bytecode::new_raw(vec![0x60, 0, 0x60, 0, 0x60, 0, 0xf0, 0x50, 0x00].into());
    let db = database(code);
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());
    txs[11] = authority_tx(1);
    txs[12] = authority_tx(2);

    let (outcomes, _) = execute_block(db, txs, DelegatedSafetyConfig::disabled(), false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(_)));
    assert_eq!(outcomes[11], TxExecutionOutcome::Skipped(SkipReason::NonceTooLow));
    assert!(matches!(outcomes[12], TxExecutionOutcome::Executed(_)));
}

#[test]
fn create_from_non_delegated_child_context_remains_allowed() {
    let create_code = Bytecode::new_raw(vec![0x60, 0, 0x60, 0, 0x60, 0, 0xf0, 0x50, 0x00].into());
    let mut db = database(zero_value_call_code(contract_target()));
    insert_contract(&mut db, contract_target(), create_code);
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());

    let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Success { .. })));
    assert_eq!(final_info(&db, &bundle, contract_target()).nonce, 2);
}

#[test]
fn reserve_violation_reverts_effects_but_keeps_authorization_nonce_and_fee() {
    let db = database(drain_code(receiver()));
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());
    txs[11] = authority_tx(1);

    let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Revert { .. })));
    assert!(matches!(outcomes[11], TxExecutionOutcome::Executed(_)));
    assert_eq!(final_info(&db, &bundle, authority()).nonce, 2);
    assert_eq!(
        final_info(&db, &bundle, authority()).balance,
        U256::from(ONE_ETHER - TRANSFER_GAS_LIMIT as u128)
    );
    assert!(
        final_info(&db, &bundle, account::mock_eoa_address(10)).balance < U256::from(ONE_ETHER)
    );
    assert_eq!(final_info(&db, &bundle, receiver()).balance, U256::from(ONE_ETHER));
}

#[test]
fn exact_reserve_boundary_is_allowed() {
    let db = database(leave_reserve_code(receiver()));
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());

    let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Success { .. })));
    assert_eq!(final_info(&db, &bundle, authority()).balance, U256::from(RESERVE));
}

#[test]
fn reverted_inner_debit_does_not_create_false_reserve_violation() {
    let mut db = database(drain_code(contract_target()));
    insert_contract(
        &mut db,
        contract_target(),
        Bytecode::new_raw(vec![0x60, 0, 0x60, 0, 0xfd].into()),
    );
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());

    let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Success { .. })));
    assert_eq!(final_info(&db, &bundle, authority()).balance, U256::from(ONE_ETHER));
}

#[test]
fn selfdestruct_is_subject_to_the_same_reserve_policy() {
    let db = database(selfdestruct_code(receiver()));
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());
    txs[11] = authority_tx(1);

    let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Revert { .. })));
    assert!(matches!(outcomes[11], TxExecutionOutcome::Executed(_)));
    assert_eq!(
        final_info(&db, &bundle, authority()).balance,
        U256::from(ONE_ETHER - TRANSFER_GAS_LIMIT as u128)
    );
}

#[test]
fn parallel_and_forced_sequential_results_are_identical() {
    let db = database(drain_code(receiver()));
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());
    txs[11] = authority_tx(1);

    let (parallel_outcomes, parallel_bundle) = execute_safety(db.clone(), txs.clone(), false);
    let (sequential_outcomes, sequential_bundle) = execute_safety(db, txs, true);
    assert_eq!(parallel_outcomes, sequential_outcomes);
    execute::compare_bundle_state(&sequential_bundle, &parallel_bundle);
}
