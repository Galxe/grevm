#![allow(missing_docs)]

//! End-to-end tests for grevm's opt-in EIP-7702 delegated-account safety policy.

use alloy_evm::precompiles::DynPrecompile;
use grevm::{
    DelegatedSafetyConfig, GrevmConfig, InvalidTransaction, ParallelState, ParallelTakeBundle,
    Scheduler, TxExecutionOutcome,
    test_utils::{
        TRANSFER_GAS_LIMIT,
        common::{account, execute, storage::InMemoryDB},
    },
};
use revm::precompile::{PrecompileId, PrecompileOutput};
use revm_context::{
    BlockEnv, CfgEnv, TxEnv,
    either::Either,
    result::ExecutionResult,
    transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
};
use revm_database::{PlainAccount, states::bundle_state::BundleRetention};
use revm_primitives::{
    Address, Bytes, HashMap, KECCAK_EMPTY, TxKind, U256, alloy_primitives::U160, hardfork::SpecId,
};
use revm_state::{AccountInfo, Bytecode};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

const BLOCK_SIZE: usize = 100;
const ONE_ETHER: u128 = 1_000_000_000_000_000_000;

fn authority() -> Address {
    Address::from(U160::from(900_000))
}

fn receiver() -> Address {
    Address::from(U160::from(900_001))
}

fn reserve_authority() -> Address {
    Address::from(U160::from(900_002))
}

fn delegate_target() -> Address {
    Address::from(U160::from(910_000))
}

fn contract_target() -> Address {
    Address::from(U160::from(910_001))
}

fn reserve_delegate_target() -> Address {
    Address::from(U160::from(910_002))
}

fn eoa_account(balance: u128, nonce: u64) -> PlainAccount {
    PlainAccount {
        info: AccountInfo {
            balance: U256::from(balance),
            nonce,
            code_hash: KECCAK_EMPTY,
            code: None,
            ..Default::default()
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

fn delegate_authority(db: &mut InMemoryDB) {
    let designator = Bytecode::new_eip7702(delegate_target());
    db.bytecodes.insert(designator.hash_slow(), designator.clone());
    let account = &mut db.accounts.get_mut(&authority()).unwrap().info;
    account.code_hash = designator.hash_slow();
    account.code = Some(designator);
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

fn delegated_tx_for(
    index: usize,
    authority: Address,
    code_address: Address,
    authorization_nonce: u64,
) -> TxEnv {
    let authorization =
        Authorization { chain_id: U256::ZERO, address: code_address, nonce: authorization_nonce };
    TxEnv {
        tx_type: 4,
        caller: account::mock_eoa_address(index),
        kind: TxKind::Call(authority),
        gas_limit: 400_000,
        gas_price: 1,
        nonce: 1,
        authorization_list: vec![Either::Right(RecoveredAuthorization::new_unchecked(
            authorization,
            RecoveredAuthority::Valid(authority),
        ))],
        ..Default::default()
    }
}

fn delegated_tx(index: usize, code_address: Address) -> TxEnv {
    delegated_tx_for(index, authority(), code_address, 0)
}

fn account_tx(caller: Address, nonce: u64, target: Address) -> TxEnv {
    TxEnv {
        caller,
        kind: TxKind::Call(target),
        gas_limit: TRANSFER_GAS_LIMIT,
        gas_price: 1,
        nonce,
        ..Default::default()
    }
}

fn authority_tx(nonce: u64) -> TxEnv {
    account_tx(authority(), nonce, receiver())
}

fn ordinary_call_tx(index: usize, target: Address) -> TxEnv {
    TxEnv {
        caller: account::mock_eoa_address(index),
        kind: TxKind::Call(target),
        gas_limit: 400_000,
        gas_price: 1,
        nonce: 1,
        ..Default::default()
    }
}

fn create_opcode_code(opcode: u8) -> Bytecode {
    let code = match opcode {
        0xf0 => vec![
            0x60, 0x00, // size
            0x60, 0x00, // offset
            0x60, 0x00, // value
            0xf0, // CREATE
            0x50, // POP
            0x00, // STOP
        ],
        0xf5 => vec![
            0x60, 0x00, // salt
            0x60, 0x00, // size
            0x60, 0x00, // offset
            0x60, 0x00, // value
            0xf5, // CREATE2
            0x50, // POP
            0x00, // STOP
        ],
        _ => unreachable!("only CREATE and CREATE2 are supported"),
    };
    Bytecode::new_raw(code.into())
}

fn call_code(target: Address, value_opcode: &[u8]) -> Bytecode {
    let mut code = Vec::new();
    append_call(&mut code, target, value_opcode);
    code.push(0x00); // STOP
    Bytecode::new_raw(code.into())
}

fn append_call(code: &mut Vec<u8>, target: Address, value_opcode: &[u8]) {
    code.extend_from_slice(&[
        0x60, 0x00, // return size
        0x60, 0x00, // return offset
        0x60, 0x00, // args size
        0x60, 0x00, // args offset
    ]);
    code.extend_from_slice(value_opcode);
    code.push(0x73); // PUSH20
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[0x62, 0x0f, 0x42, 0x40, 0xf1, 0x50]); // CALL, POP
}

fn signal_then_value_call_code(signal: Address, target: Address, value: u64) -> Bytecode {
    let mut code = Vec::new();
    append_call(&mut code, signal, &[0x60, 0x00]);
    let mut value_opcode = vec![0x67]; // PUSH8
    value_opcode.extend_from_slice(&value.to_be_bytes());
    append_call(&mut code, target, &value_opcode);
    code.push(0x00); // STOP
    Bytecode::new_raw(code.into())
}

fn drain_code(target: Address) -> Bytecode {
    call_code(target, &[0x47]) // SELFBALANCE
}

fn fixed_value_call_code(target: Address, value: u64) -> Bytecode {
    let mut push = vec![0x67]; // PUSH8
    push.extend_from_slice(&value.to_be_bytes());
    call_code(target, &push)
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
    execute_block_with_precompiles(db, txs, safety, force_sequential, 23, None)
}

fn execute_block_with_precompiles(
    db: InMemoryDB,
    txs: Vec<TxEnv>,
    safety: DelegatedSafetyConfig,
    force_sequential: bool,
    concurrency_level: usize,
    custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
) -> (Vec<TxExecutionOutcome>, revm_database::BundleState) {
    let txs = Arc::new(txs);
    let block = BlockEnv { beneficiary: account::MINER_ADDRESS, ..Default::default() };
    let state = ParallelState::new(Arc::new(db), true, true);
    let config = GrevmConfig {
        concurrency_level,
        force_sequential,
        min_parallel_txs: 0,
        delegated_safety: safety,
    };
    let scheduler = Scheduler::new_with_runtime_config(
        CfgEnv::new_with_spec(SpecId::PRAGUE),
        block,
        txs,
        state,
        custom_precompiles,
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
    execute_block(db, txs, DelegatedSafetyConfig::enabled(), force_sequential)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutcomeKind {
    Success,
    Revert,
    Halt,
    NonceTooLow,
    LackOfFund,
}

fn assert_outcome(outcome: &TxExecutionOutcome, expected: OutcomeKind, context: &str) {
    let matches = matches!(
        (outcome, expected),
        (TxExecutionOutcome::Executed(ExecutionResult::Success { .. }), OutcomeKind::Success) |
            (TxExecutionOutcome::Executed(ExecutionResult::Revert { .. }), OutcomeKind::Revert) |
            (TxExecutionOutcome::Executed(ExecutionResult::Halt { .. }), OutcomeKind::Halt) |
            (
                TxExecutionOutcome::Skipped(InvalidTransaction::NonceTooLow { .. }),
                OutcomeKind::NonceTooLow
            ) |
            (
                TxExecutionOutcome::Skipped(InvalidTransaction::LackOfFundForMaxFee { .. }),
                OutcomeKind::LackOfFund
            )
    );
    assert!(matches, "{context}: expected {expected:?}, got {outcome:?}");
}

fn execute_in_both_modes(
    db: InMemoryDB,
    txs: Vec<TxEnv>,
    safety: DelegatedSafetyConfig,
) -> (Vec<TxExecutionOutcome>, revm_database::BundleState) {
    let parallel = execute_block(db.clone(), txs.clone(), safety, false);
    let sequential = execute_block(db, txs, safety, true);
    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    parallel
}

fn retry_probe_precompiles(
    executions: Arc<AtomicUsize>,
    blocker: Address,
    probe: Address,
    coordinate_parallel_attempt: bool,
) -> Arc<Vec<(Address, DynPrecompile)>> {
    let blocker_executions = executions.clone();
    let blocker_precompile = DynPrecompile::new_stateful(
        PrecompileId::Custom("grevm-test-retry-blocker".into()),
        move |input| {
            if coordinate_parallel_attempt {
                while blocker_executions.load(Ordering::Acquire) == 0 {
                    thread::yield_now();
                }
                // The probe runs near the start of the delegated call. Keep tx 0 open long enough
                // for tx 1 to publish its stale speculative result before tx 0 publishes its write.
                thread::sleep(Duration::from_millis(50));
            }
            Ok(PrecompileOutput::new(0, Bytes::new(), input.reservoir))
        },
    );
    let probe_precompile = DynPrecompile::new_stateful(
        PrecompileId::Custom("grevm-test-retry-probe".into()),
        move |input| {
            executions.fetch_add(1, Ordering::AcqRel);
            Ok(PrecompileOutput::new(0, Bytes::new(), input.reservoir))
        },
    );
    Arc::new(vec![(blocker, blocker_precompile), (probe, probe_precompile)])
}

#[test]
fn delegated_create_and_balance_transfer_follow_four_policy_combinations() {
    let policy_cases = [
        ("disabled", DelegatedSafetyConfig::disabled()),
        ("create-only", DelegatedSafetyConfig::create_only()),
        ("reserve-only", DelegatedSafetyConfig::reserve_only()),
        ("enabled", DelegatedSafetyConfig::enabled()),
    ];

    for (opcode_name, opcode) in [("CREATE", 0xf0), ("CREATE2", 0xf5)] {
        for (policy_name, safety) in policy_cases {
            // Two independent delegated accounts make both policy decisions observable in the
            // same block. A executes CREATE/CREATE2; B drains its balance before a later B tx.
            let mut db = database(create_opcode_code(opcode));
            db.accounts.insert(reserve_authority(), eoa_account(ONE_ETHER, 0));
            insert_contract(&mut db, reserve_delegate_target(), drain_code(receiver()));

            let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
            txs[10] = delegated_tx(10, delegate_target());
            txs[20] = authority_tx(1);
            txs[21] = authority_tx(2);
            txs[30] = delegated_tx_for(30, reserve_authority(), reserve_delegate_target(), 0);
            txs[31] = account_tx(reserve_authority(), 1, receiver());

            let context = format!("{opcode_name}/{policy_name}");
            let (outcomes, bundle) = execute_in_both_modes(db.clone(), txs, safety);

            let create_kind = if safety.forbid_delegated_create {
                OutcomeKind::Halt
            } else {
                OutcomeKind::Success
            };
            assert_outcome(&outcomes[10], create_kind, &format!("{context}: delegated create"));
            let stale_nonce_kind = if safety.forbid_delegated_create {
                OutcomeKind::Success
            } else {
                OutcomeKind::NonceTooLow
            };
            assert_outcome(&outcomes[20], stale_nonce_kind, &format!("{context}: nonce 1 suffix"));
            assert_outcome(
                &outcomes[21],
                OutcomeKind::Success,
                &format!("{context}: nonce 2 suffix"),
            );

            let transfer_kind = if safety.reserve_delegated_balance {
                OutcomeKind::Revert
            } else {
                OutcomeKind::Success
            };
            assert_outcome(
                &outcomes[30],
                transfer_kind,
                &format!("{context}: delegated balance drain"),
            );
            let funded_suffix_kind = if safety.reserve_delegated_balance {
                OutcomeKind::Success
            } else {
                OutcomeKind::LackOfFund
            };
            assert_outcome(
                &outcomes[31],
                funded_suffix_kind,
                &format!("{context}: balance-dependent suffix"),
            );

            assert_eq!(final_info(&db, &bundle, authority()).nonce, 3, "{context}");
            let expected_reserve_nonce = if safety.reserve_delegated_balance { 2 } else { 1 };
            assert_eq!(
                final_info(&db, &bundle, reserve_authority()).nonce,
                expected_reserve_nonce,
                "{context}"
            );
            let expected_reserve_balance = if safety.reserve_delegated_balance {
                U256::from(ONE_ETHER - TRANSFER_GAS_LIMIT as u128)
            } else {
                U256::ZERO
            };
            assert_eq!(
                final_info(&db, &bundle, reserve_authority()).balance,
                expected_reserve_balance,
                "{context}"
            );
            let expected_receiver_balance = if safety.reserve_delegated_balance {
                U256::from(ONE_ETHER)
            } else {
                U256::from(2 * ONE_ETHER)
            };
            assert_eq!(
                final_info(&db, &bundle, receiver()).balance,
                expected_receiver_balance,
                "{context}"
            );
            assert!(
                final_info(&db, &bundle, account::mock_eoa_address(30)).balance <
                    U256::from(ONE_ETHER),
                "{context}: the sponsored delegated attempt must remain charged"
            );
        }
    }
}

#[test]
fn reserve_retry_reexecutes_after_a_stale_speculative_balance_read() {
    const INITIAL_BALANCE: u64 = 150;
    const EARLIER_ROOT_TRANSFER: u64 = 100;
    const DELEGATED_TRANSFER: u64 = 120;
    const FUTURE_VALUE: u64 = 40;

    let blocker = Address::from(U160::from(920_000));
    let probe = Address::from(U160::from(920_001));
    let mut db = database(signal_then_value_call_code(probe, receiver(), DELEGATED_TRANSFER));
    delegate_authority(&mut db);
    db.accounts.get_mut(&authority()).unwrap().info.balance = U256::from(INITIAL_BALANCE);

    let mut earlier = account_tx(authority(), 0, blocker);
    earlier.value = U256::from(EARLIER_ROOT_TRANSFER);
    earlier.gas_price = 0;
    let mut delegated = ordinary_call_tx(1, authority());
    delegated.gas_price = 0;
    let mut future = account_tx(authority(), 1, receiver());
    future.value = U256::from(FUTURE_VALUE);
    future.gas_price = 0;
    let txs = vec![earlier, delegated, future];

    // tx 0 blocks inside a custom precompile until tx 1 has entered its delegated code. Tx 1
    // therefore first reads A.balance=150 and tentatively hits the reserve guard after sending
    // 120. Once tx 0 publishes its root transfer, MV validation invalidates that incarnation.
    // Re-execution sees A.balance=50, so the 120-value CALL fails without a debit and tx 1
    // succeeds.
    let parallel_executions = Arc::new(AtomicUsize::new(0));
    let parallel = execute_block_with_precompiles(
        db.clone(),
        txs.clone(),
        DelegatedSafetyConfig::reserve_only(),
        false,
        2,
        Some(retry_probe_precompiles(parallel_executions.clone(), blocker, probe, true)),
    );
    assert!(
        parallel_executions.load(Ordering::Acquire) >= 2,
        "the delegated transaction must execute speculatively and then retry"
    );

    let sequential_executions = Arc::new(AtomicUsize::new(0));
    let sequential = execute_block_with_precompiles(
        db.clone(),
        txs,
        DelegatedSafetyConfig::reserve_only(),
        true,
        1,
        Some(retry_probe_precompiles(sequential_executions.clone(), blocker, probe, false)),
    );
    assert_eq!(
        sequential_executions.load(Ordering::Acquire),
        1,
        "the ordered reference executes the delegated transaction once"
    );

    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    assert_outcome(&parallel.0[0], OutcomeKind::Success, "earlier root transfer");
    assert_outcome(&parallel.0[1], OutcomeKind::Success, "retried delegated transaction");
    assert_outcome(&parallel.0[2], OutcomeKind::Success, "funded suffix");
    assert_eq!(final_info(&db, &parallel.1, authority()).balance, U256::from(10));
}

#[test]
fn ordinary_create_and_create2_remain_allowed_with_both_policies_enabled() {
    for (opcode_name, opcode) in [("CREATE", 0xf0), ("CREATE2", 0xf5)] {
        for through_delegate in [false, true] {
            let target_code = if through_delegate {
                zero_value_call_code(contract_target())
            } else {
                Bytecode::new_raw(vec![0x00].into())
            };
            let mut db = database(target_code);
            insert_contract(&mut db, contract_target(), create_opcode_code(opcode));
            let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
            txs[10] = if through_delegate {
                delegated_tx(10, delegate_target())
            } else {
                ordinary_call_tx(10, contract_target())
            };

            let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
            let path = if through_delegate { "delegated parent" } else { "direct call" };
            assert_outcome(&outcomes[10], OutcomeKind::Success, &format!("{opcode_name}/{path}"));
            assert_eq!(
                final_info(&db, &bundle, contract_target()).nonce,
                2,
                "{opcode_name}/{path}: the ordinary contract must execute its create opcode"
            );
        }
    }
}

#[test]
fn account_without_future_transactions_can_drain_its_balance() {
    let db = database(drain_code(receiver()));
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());

    let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Success { .. })));
    assert_eq!(final_info(&db, &bundle, authority()).balance, U256::ZERO);
}

#[test]
fn ordinary_transaction_value_from_delegated_sender_is_not_reserved() {
    let mut db = database(Bytecode::new_raw(vec![0x00].into()));
    delegate_authority(&mut db);
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    let mut drain = authority_tx(0);
    drain.value = U256::from(ONE_ETHER - TRANSFER_GAS_LIMIT as u128);
    txs[10] = drain;
    txs[11] = authority_tx(1);

    let (outcomes, bundle) =
        execute_block(db.clone(), txs, DelegatedSafetyConfig::reserve_only(), false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Success { .. })));
    assert!(matches!(
        outcomes[11],
        TxExecutionOutcome::Skipped(InvalidTransaction::LackOfFundForMaxFee { .. })
    ));
    assert_eq!(final_info(&db, &bundle, authority()).balance, U256::ZERO);
    assert_eq!(
        final_info(&db, &bundle, receiver()).balance,
        U256::from(2 * ONE_ETHER - TRANSFER_GAS_LIMIT as u128)
    );
}

#[test]
fn delegated_reserve_accepts_ample_and_exact_balances_but_reverts_below_boundary() {
    // The two suffix transactions each cost exactly 100_000 value + 21_000 gas, so their
    // max_balance_spending() sum and actual total cost are both 242_000.
    for (case, drain, expected, expected_final_balance) in [
        ("ample", 700_000, OutcomeKind::Success, 58_000),
        ("exact", 758_000, OutcomeKind::Success, 0),
        ("one wei short", 758_001, OutcomeKind::Revert, 758_000),
    ] {
        let mut db = database(fixed_value_call_code(receiver(), drain));
        db.accounts.get_mut(&authority()).unwrap().info.balance = U256::from(1_000_000);
        let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
        txs[10] = delegated_tx(10, delegate_target());
        let mut first = authority_tx(1);
        first.value = U256::from(100_000);
        txs[11] = first;
        let mut second = authority_tx(2);
        second.value = U256::from(100_000);
        txs[12] = second;

        let (outcomes, bundle) = execute_safety(db.clone(), txs, false);
        assert_outcome(&outcomes[10], expected, case);
        assert_outcome(&outcomes[11], OutcomeKind::Success, case);
        assert_outcome(&outcomes[12], OutcomeKind::Success, case);
        assert_eq!(
            final_info(&db, &bundle, authority()).balance,
            U256::from(expected_final_balance),
            "{case}"
        );
    }
}

#[test]
fn disabled_reserve_allows_max_cost_overestimate_when_actual_cost_is_affordable() {
    const INITIAL_BALANCE: u64 = 1_000_000;
    const DELEGATED_SPEND: u64 = 850_000;
    const FUTURE_GAS_LIMIT: u64 = 100_000;

    let mut db = database(fixed_value_call_code(receiver(), DELEGATED_SPEND));
    db.accounts.get_mut(&authority()).unwrap().info.balance = U256::from(INITIAL_BALANCE);
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());
    for (txid, nonce) in [(11, 1), (12, 2)] {
        let mut future = authority_tx(nonce);
        future.gas_limit = FUTURE_GAS_LIMIT;
        txs[txid] = future;
    }

    // The suffix max_balance_spending() sum is 200k, but each EOA transfer actually uses only 21k.
    // Leaving 150k lets each tx separately pass its 100k upfront check, then the first refund makes
    // enough balance available for the second. The real sequential total is only 42k.
    let (disabled, disabled_bundle) =
        execute_block(db.clone(), txs.clone(), DelegatedSafetyConfig::disabled(), false);
    assert_outcome(&disabled[10], OutcomeKind::Success, "reserve disabled/delegated");
    assert_outcome(&disabled[11], OutcomeKind::Success, "reserve disabled/future 1");
    assert_outcome(&disabled[12], OutcomeKind::Success, "reserve disabled/future 2");
    assert_eq!(
        final_info(&db, &disabled_bundle, authority()).balance,
        U256::from(INITIAL_BALANCE - DELEGATED_SPEND - 2 * TRANSFER_GAS_LIMIT)
    );

    // The enabled policy intentionally uses the conservative maximum and rejects the same debit.
    let (enabled, enabled_bundle) =
        execute_block(db.clone(), txs, DelegatedSafetyConfig::reserve_only(), false);
    assert_outcome(&enabled[10], OutcomeKind::Revert, "reserve enabled/delegated");
    assert_outcome(&enabled[11], OutcomeKind::Success, "reserve enabled/future 1");
    assert_outcome(&enabled[12], OutcomeKind::Success, "reserve enabled/future 2");
    assert_eq!(
        final_info(&db, &enabled_bundle, authority()).balance,
        U256::from(INITIAL_BALANCE - 2 * TRANSFER_GAS_LIMIT)
    );
}

#[test]
fn reserve_revert_preserves_eip7702_authorization_refund() {
    let db = database(drain_code(receiver()));
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());
    txs[11] = authority_tx(1);

    // Both executions take the same EIP-7702 authorization and delegated CALL path. The enabled
    // policy changes the final outcome to REVERT, but the pre-execution authorization refund is
    // independent of the rolled-back balance transfer and therefore keeps gas usage identical.
    let (standard, _) =
        execute_in_both_modes(db.clone(), txs.clone(), DelegatedSafetyConfig::disabled());
    let (reserve, _) = execute_in_both_modes(db, txs, DelegatedSafetyConfig::reserve_only());
    assert_outcome(&standard[10], OutcomeKind::Success, "reserve disabled");
    assert_outcome(&reserve[10], OutcomeKind::Revert, "reserve enabled");
    let TxExecutionOutcome::Executed(standard) = &standard[10] else {
        unreachable!("outcome checked above")
    };
    let TxExecutionOutcome::Executed(reserve) = &reserve[10] else {
        unreachable!("outcome checked above")
    };
    assert_eq!(standard.gas(), reserve.gas());
}

#[test]
fn reserve_does_not_require_more_than_the_pre_debit_balance() {
    let mut db = database(fixed_value_call_code(contract_target(), 1));
    insert_contract(&mut db, contract_target(), selfdestruct_code(authority()));
    db.accounts.get_mut(&authority()).unwrap().info.balance = U256::from(200_000);
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());
    let mut first = authority_tx(1);
    first.value = U256::from(100_000);
    txs[11] = first;
    let mut second = authority_tx(2);
    second.value = U256::from(100_000);
    txs[12] = second;

    let (outcomes, _) = execute_safety(db, txs, false);
    // The delegated CALL debits one wei, and the callee's SELFDESTRUCT immediately returns it.
    // A starts below the conservative 242k suffix, so required=min(200k, 242k)=200k.
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Success { .. })));
    assert!(matches!(outcomes[11], TxExecutionOutcome::Executed(_)));
    assert!(matches!(
        outcomes[12],
        TxExecutionOutcome::Skipped(InvalidTransaction::LackOfFundForMaxFee { .. })
    ));
}

#[test]
fn top_level_create_calling_a_delegated_account_keeps_creator_nonce_on_reserve_revert() {
    let mut db = database(drain_code(receiver()));
    delegate_authority(&mut db);
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    let creator = account::mock_eoa_address(10);
    txs[10] = TxEnv {
        caller: creator,
        kind: TxKind::Create,
        data: zero_value_call_code(authority()).original_bytes(),
        gas_limit: 400_000,
        gas_price: 1,
        nonce: 1,
        ..Default::default()
    };
    txs[11] = authority_tx(0);

    let (outcomes, bundle) =
        execute_block(db.clone(), txs, DelegatedSafetyConfig::reserve_only(), false);
    assert!(matches!(outcomes[10], TxExecutionOutcome::Executed(ExecutionResult::Revert { .. })));
    assert!(matches!(outcomes[11], TxExecutionOutcome::Executed(_)));
    assert_eq!(final_info(&db, &bundle, creator).nonce, 2);
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
fn reserve_tracking_does_not_change_selfdestruct_gas() {
    let db = database(selfdestruct_code(receiver()));
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = delegated_tx(10, delegate_target());

    let (standard, standard_bundle) =
        execute_block(db.clone(), txs.clone(), DelegatedSafetyConfig::disabled(), false);
    let (reserve, reserve_bundle) =
        execute_block(db, txs, DelegatedSafetyConfig::reserve_only(), false);
    let TxExecutionOutcome::Executed(standard) = &standard[10] else {
        panic!("selfdestruct transaction must execute on the standard path")
    };
    let TxExecutionOutcome::Executed(reserve) = &reserve[10] else {
        panic!("selfdestruct transaction must execute on the reserve path")
    };
    assert_eq!(standard.gas(), reserve.gas());
    execute::compare_bundle_state(&standard_bundle, &reserve_bundle);
}

#[test]
fn reserve_handler_preserves_full_result_gas_when_eip7623_floor_applies() {
    let db = database(Bytecode::new());
    let mut txs: Vec<_> = (0..BLOCK_SIZE).map(padding_tx).collect();
    txs[10] = TxEnv {
        data: Bytes::from(vec![1; 1_000]),
        gas_limit: 100_000,
        value: U256::ZERO,
        ..padding_tx(10)
    };

    let (standard, _) =
        execute_in_both_modes(db.clone(), txs.clone(), DelegatedSafetyConfig::disabled());
    let (reserve, _) = execute_in_both_modes(db, txs, DelegatedSafetyConfig::reserve_only());
    let TxExecutionOutcome::Executed(standard) = &standard[10] else {
        panic!("floor transaction must execute on the standard path")
    };
    let TxExecutionOutcome::Executed(reserve) = &reserve[10] else {
        panic!("floor transaction must execute on the reserve path")
    };

    assert_eq!(standard.tx_gas_used(), standard.gas().floor_gas());
    assert_eq!(standard.gas(), reserve.gas());
}
