#![allow(missing_docs)]

//! End-to-end tests for capability-restricted custom precompiles.

use grevm::{
    DelegatedSafetyConfig, DynParallelPrecompile, GrevmConfig, InvalidTransaction, ParallelState,
    ParallelTakeBundle, Scheduler, TxExecutionOutcome,
    test_utils::common::{account, execute, storage::InMemoryDB},
};
use revm::precompile::{PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput};
use revm_context::{
    BlockEnv, CfgEnv, TxEnv,
    result::{EVMError, ExecutionResult},
};
use revm_database::{
    PlainAccount,
    states::{BundleState, bundle_state::BundleRetention},
};
use revm_primitives::{
    Address, Bytes, HashMap, KECCAK_EMPTY, TxKind, U256, alloy_primitives::U160, hardfork::SpecId,
};
use revm_state::{AccountInfo, Bytecode};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const GAS_LIMIT: u64 = 500_000;
const TX_BASE_GAS: u64 = 21_000;
const SLOT_0: U256 = U256::ZERO;
const SLOT_1: U256 = U256::from_limbs([1, 0, 0, 0]);

fn address(index: usize) -> Address {
    Address::from(U160::from(950_000 + index))
}

fn read_your_writes_precompile() -> Address {
    address(0)
}

fn nested_write_precompile() -> Address {
    address(1)
}

fn static_storage_precompile() -> Address {
    address(2)
}

fn static_balance_precompile() -> Address {
    address(3)
}

fn fallback_precompile() -> Address {
    address(4)
}

fn beneficiary_predecessor_precompile() -> Address {
    address(5)
}

fn beneficiary_reader_precompile() -> Address {
    address(6)
}

fn reverting_precompile() -> Address {
    address(7)
}

fn halting_precompile() -> Address {
    address(8)
}

fn fatal_precompile() -> Address {
    address(9)
}

fn read_your_writes_contract() -> Address {
    address(10)
}

fn nested_outer_contract() -> Address {
    address(11)
}

fn nested_inner_contract() -> Address {
    address(12)
}

fn static_caller_contract() -> Address {
    address(13)
}

fn failing_precompile_caller_contract() -> Address {
    address(14)
}

fn state_holder() -> Address {
    address(20)
}

fn plain_account(balance: u64, storage: impl IntoIterator<Item = (U256, U256)>) -> PlainAccount {
    PlainAccount {
        info: AccountInfo {
            balance: U256::from(balance),
            nonce: 1,
            code_hash: KECCAK_EMPTY,
            code: None,
            ..Default::default()
        },
        storage: storage.into_iter().collect(),
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

fn insert_contract(db: &mut InMemoryDB, address: Address, code: Bytecode) {
    db.accounts.insert(address, contract_account(&code));
    db.bytecodes.insert(code.hash_slow(), code);
}

fn database() -> InMemoryDB {
    InMemoryDB::new(account::mock_block_accounts(8), HashMap::default(), HashMap::default())
}

fn append_call(code: &mut Vec<u8>, target: Address) {
    // CALL stack, from bottom to top: out size/offset, input size/offset, value, target, gas.
    for _ in 0..5 {
        code.extend_from_slice(&[0x60, 0x00]);
    }
    code.push(0x73); // PUSH20 target
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[0x62, 0x00, 0xc3, 0x50, 0xf1]); // PUSH3 50_000; CALL
}

fn append_staticcall(code: &mut Vec<u8>, target: Address) {
    // STATICCALL has the same arguments as CALL except for value.
    for _ in 0..4 {
        code.extend_from_slice(&[0x60, 0x00]);
    }
    code.push(0x73); // PUSH20 target
    code.extend_from_slice(target.as_slice());
    code.extend_from_slice(&[0x62, 0x00, 0xc3, 0x50, 0xfa]); // PUSH3 50_000; STATICCALL
}

fn call_tx(index: usize, target: Address, nonce: u64) -> TxEnv {
    TxEnv {
        caller: account::mock_eoa_address(index),
        kind: TxKind::Call(target),
        gas_limit: GAS_LIMIT,
        gas_price: 0,
        nonce,
        ..Default::default()
    }
}

fn fee_paying_call_tx(index: usize, target: Address, nonce: u64) -> TxEnv {
    let mut tx = call_tx(index, target, nonce);
    tx.gas_price = 1;
    tx
}

fn successful_output(reservoir: u64) -> PrecompileOutput {
    PrecompileOutput::new(0, Bytes::new(), reservoir)
}

#[derive(Clone, Default)]
struct ReadTrace {
    calls: Arc<AtomicUsize>,
    observations: Arc<StdMutex<Vec<U256>>>,
}

impl ReadTrace {
    fn record(&self, value: U256) {
        self.observations.lock().unwrap().push(value);
        self.calls.fetch_add(1, Ordering::Release);
    }

    fn wait_for_first(&self, context: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.calls.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "timed out waiting for {context}");
            std::thread::yield_now();
        }
    }

    fn observations(&self) -> Vec<U256> {
        self.observations.lock().unwrap().clone()
    }
}

fn execute_block(
    db: InMemoryDB,
    txs: Vec<TxEnv>,
    precompiles: Arc<Vec<(Address, DynParallelPrecompile)>>,
    force_sequential: bool,
) -> (Vec<TxExecutionOutcome>, BundleState, BTreeMap<&'static str, usize>) {
    let scheduler = Scheduler::new_with_runtime_config(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv { beneficiary: account::MINER_ADDRESS, ..Default::default() },
        Arc::new(txs),
        ParallelState::new(Arc::new(db), true, true),
        Some(precompiles),
        GrevmConfig {
            concurrency_level: 2,
            force_sequential,
            min_parallel_txs: 0,
            delegated_safety: DelegatedSafetyConfig::disabled(),
        },
    );
    scheduler.execute().expect("block execution must succeed");
    let metrics = scheduler.metrics_snapshot();
    let (outcomes, mut state) = scheduler.take_result_and_state();
    let bundle = state.parallel_take_bundle(BundleRetention::Reverts);
    (outcomes, bundle, metrics)
}

fn final_storage(db: &InMemoryDB, bundle: &BundleState, address: Address, slot: U256) -> U256 {
    bundle
        .state
        .get(&address)
        .and_then(|account| account.storage_slot(slot))
        .or_else(|| {
            db.accounts.get(&address).and_then(|account| account.storage.get(&slot).copied())
        })
        .unwrap_or_default()
}

fn final_balance(db: &InMemoryDB, bundle: &BundleState, address: Address) -> U256 {
    bundle
        .state
        .get(&address)
        .and_then(|account| account.info.as_ref())
        .map(|info| info.balance)
        .or_else(|| db.accounts.get(&address).map(|account| account.info.balance))
        .unwrap_or_default()
}

fn assert_success(outcome: &TxExecutionOutcome) {
    assert!(
        matches!(outcome, TxExecutionOutcome::Executed(ExecutionResult::Success { .. })),
        "expected success, got {outcome:?}"
    );
}

#[test]
fn precompile_sload_observes_an_earlier_sstore_in_the_same_transaction() {
    const OLD_VALUE: u64 = 7;
    const NEW_VALUE: u8 = 42;

    let mut code = vec![
        0x60, NEW_VALUE, // PUSH1 new value
        0x60, 0x00, // PUSH1 slot 0
        0x55, // SSTORE
    ];
    append_call(&mut code, read_your_writes_precompile());
    code.extend_from_slice(&[0x50, 0x00]); // POP success; STOP

    let mut db = database();
    insert_contract(&mut db, read_your_writes_contract(), Bytecode::new_raw(code.into()));
    db.accounts
        .get_mut(&read_your_writes_contract())
        .unwrap()
        .storage
        .insert(SLOT_0, U256::from(OLD_VALUE));

    let precompile = DynParallelPrecompile::new(
        PrecompileId::Custom("grevm-test-read-your-writes".into()),
        |input| {
            let caller = input.caller();
            let reservoir = input.reservoir();
            let value = input.state().sload(caller, SLOT_0)?.data;
            input.state().sstore(caller, SLOT_1, value)?;
            Ok(successful_output(reservoir))
        },
    );
    let precompiles = Arc::new(vec![(read_your_writes_precompile(), precompile)]);
    let txs = vec![call_tx(0, read_your_writes_contract(), 1)];

    let parallel = execute_block(db.clone(), txs.clone(), precompiles.clone(), false);
    let sequential = execute_block(db.clone(), txs, precompiles, true);
    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    assert_success(&parallel.0[0]);
    assert_eq!(
        final_storage(&db, &parallel.1, read_your_writes_contract(), SLOT_0),
        U256::from(NEW_VALUE)
    );
    assert_eq!(
        final_storage(&db, &parallel.1, read_your_writes_contract(), SLOT_1),
        U256::from(NEW_VALUE),
        "the precompile must read the journal value, not the old backing-DB value"
    );
}

#[test]
fn reverting_an_enclosing_call_frame_rolls_back_precompile_writes() {
    const OLD_VALUE: u64 = 11;
    const TENTATIVE_VALUE: u64 = 99;

    let mut inner = Vec::new();
    append_call(&mut inner, nested_write_precompile());
    inner.extend_from_slice(&[
        0x50, // POP precompile success
        0x60, 0x00, 0x60, 0x00, 0xfd, // REVERT(0, 0)
    ]);
    let mut outer = Vec::new();
    append_call(&mut outer, nested_inner_contract());
    outer.extend_from_slice(&[
        0x15, // ISZERO: prove the outer frame observed and caught the inner revert
        0x60, 0x00, // PUSH1 marker slot
        0x55, // SSTORE
        0x00, // STOP
    ]);

    let mut db = database();
    insert_contract(&mut db, nested_inner_contract(), Bytecode::new_raw(inner.into()));
    insert_contract(&mut db, nested_outer_contract(), Bytecode::new_raw(outer.into()));
    db.accounts.insert(state_holder(), plain_account(123, [(SLOT_0, U256::from(OLD_VALUE))]));

    let precompile = DynParallelPrecompile::new(
        PrecompileId::Custom("grevm-test-nested-revert".into()),
        |input| {
            let reservoir = input.reservoir();
            input.state().sstore(state_holder(), SLOT_0, U256::from(TENTATIVE_VALUE))?;
            Ok(successful_output(reservoir))
        },
    );
    let precompiles = Arc::new(vec![(nested_write_precompile(), precompile)]);
    let txs = vec![call_tx(0, nested_outer_contract(), 1)];

    let parallel = execute_block(db.clone(), txs.clone(), precompiles.clone(), false);
    let sequential = execute_block(db.clone(), txs, precompiles, true);
    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    assert_success(&parallel.0[0]);
    assert_eq!(
        final_storage(&db, &parallel.1, state_holder(), SLOT_0),
        U256::from(OLD_VALUE),
        "the inner frame's committed precompile call must still revert with its parent frame"
    );
    assert_eq!(
        final_storage(&db, &parallel.1, nested_outer_contract(), SLOT_0),
        U256::from(1),
        "the outer frame must catch the inner revert and continue"
    );
}

#[test]
fn staticcall_rejects_every_exposed_state_mutation_before_it_changes_state() {
    const OLD_BALANCE: u64 = 123;
    const NEW_BALANCE: u64 = 456;
    const OLD_STORAGE: u64 = 17;
    const NEW_STORAGE: u64 = 18;

    let mut code = Vec::new();
    append_staticcall(&mut code, static_storage_precompile());
    code.extend_from_slice(&[0x15, 0x60, 0x00, 0x55]); // store ISZERO(success) in slot 0
    append_staticcall(&mut code, static_balance_precompile());
    code.extend_from_slice(&[0x15, 0x60, 0x01, 0x55, 0x00]); // slot 1; STOP

    let mut db = database();
    insert_contract(&mut db, static_caller_contract(), Bytecode::new_raw(code.into()));
    db.accounts
        .insert(state_holder(), plain_account(OLD_BALANCE, [(SLOT_0, U256::from(OLD_STORAGE))]));

    let storage = DynParallelPrecompile::new(
        PrecompileId::Custom("grevm-test-static-sstore".into()),
        |input| {
            assert!(input.is_static());
            let reservoir = input.reservoir();
            // Deliberately ignore the facade error: the adapter's sticky fault must still make
            // the call fail closed instead of accepting the returned success.
            let _ = input.state().sstore(state_holder(), SLOT_0, U256::from(NEW_STORAGE));
            Ok(successful_output(reservoir))
        },
    );
    let balance = DynParallelPrecompile::new(
        PrecompileId::Custom("grevm-test-static-set-balance".into()),
        |input| {
            assert!(input.is_static());
            let reservoir = input.reservoir();
            // Exercise the same sticky fail-closed behavior for every exposed mutation API.
            let _ = input.state().set_balance(state_holder(), U256::from(NEW_BALANCE));
            Ok(successful_output(reservoir))
        },
    );
    let precompiles = Arc::new(vec![
        (static_storage_precompile(), storage),
        (static_balance_precompile(), balance),
    ]);
    let txs = vec![call_tx(0, static_caller_contract(), 1)];

    let parallel = execute_block(db.clone(), txs.clone(), precompiles.clone(), false);
    let sequential = execute_block(db.clone(), txs, precompiles, true);
    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    assert_success(&parallel.0[0]);
    assert_eq!(final_storage(&db, &parallel.1, state_holder(), SLOT_0), U256::from(OLD_STORAGE));
    assert_eq!(final_balance(&db, &parallel.1, state_holder()), U256::from(OLD_BALANCE));
    assert_eq!(
        final_storage(&db, &parallel.1, static_caller_contract(), SLOT_0),
        U256::from(1),
        "STATICCALL must report failure for sstore"
    );
    assert_eq!(
        final_storage(&db, &parallel.1, static_caller_contract(), SLOT_1),
        U256::from(1),
        "STATICCALL must report failure for set_balance"
    );
}

#[test]
fn nonce_mismatch_suffix_fallback_keeps_restricted_precompiles_installed() {
    const WRITTEN_VALUE: u64 = 77;

    let mut db = database();
    db.accounts.insert(state_holder(), plain_account(0, []));
    let precompile = DynParallelPrecompile::new(
        PrecompileId::Custom("grevm-test-suffix-fallback".into()),
        |input| {
            let reservoir = input.reservoir();
            input.state().sstore(state_holder(), SLOT_0, U256::from(WRITTEN_VALUE))?;
            Ok(successful_output(reservoir))
        },
    );
    let precompiles = Arc::new(vec![(fallback_precompile(), precompile)]);
    let caller = account::mock_eoa_address(0);
    let txs = vec![
        call_tx(0, caller, 1),
        // tx 0 increments this sender to nonce 2. This stale nonce is only classified after the
        // committed prefix forces ordered revalidation of the suffix.
        call_tx(0, caller, 1),
        call_tx(1, fallback_precompile(), 1),
    ];

    let parallel = execute_block(db.clone(), txs.clone(), precompiles.clone(), false);
    let sequential = execute_block(db.clone(), txs, precompiles, true);
    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    assert_success(&parallel.0[0]);
    assert!(matches!(
        parallel.0[1],
        TxExecutionOutcome::Skipped(InvalidTransaction::NonceTooLow { .. })
    ));
    assert_success(&parallel.0[2]);
    assert!(
        parallel.2["grevm.execution_cnt"] > 3,
        "parallel execution plus suffix replay must execute more than the three ordered txs"
    );
    assert_eq!(
        final_storage(&db, &parallel.1, state_holder(), SLOT_0),
        U256::from(WRITTEN_VALUE),
        "the suffix replay EVM must install the same restricted precompile adapter"
    );
}

#[test]
fn beneficiary_reward_read_retries_and_charges_precompile_gas_once() {
    const PREDECESSOR_GAS: u64 = 1_234;
    const READER_GAS: u64 = 567;

    let precompiles = |trace: ReadTrace, coordinate_parallel_attempt: bool| {
        let predecessor_trace = trace.clone();
        let predecessor = DynParallelPrecompile::new(
            PrecompileId::Custom("grevm-test-beneficiary-reward-predecessor".into()),
            move |input| {
                if coordinate_parallel_attempt {
                    // Keep tx 0's positive reward deferred and unresolved until tx 1 has observed
                    // the block-start beneficiary balance in its first speculative incarnation.
                    predecessor_trace.wait_for_first("the beneficiary balance read");
                }
                Ok(PrecompileOutput::new(PREDECESSOR_GAS, Bytes::new(), input.reservoir()))
            },
        );
        let reader = DynParallelPrecompile::new(
            PrecompileId::Custom("grevm-test-beneficiary-reward-reader".into()),
            move |input| {
                let reservoir = input.reservoir();
                let balance = input.state().balance(account::MINER_ADDRESS)?.data;
                input.state().sstore(state_holder(), SLOT_0, balance)?;
                // Publish only after the state read so tx 0 cannot finish before the first
                // incarnation has anchored itself to the unresolved beneficiary history.
                trace.record(balance);
                Ok(PrecompileOutput::new(READER_GAS, Bytes::new(), reservoir))
            },
        );
        Arc::new(vec![
            (beneficiary_predecessor_precompile(), predecessor),
            (beneficiary_reader_precompile(), reader),
        ])
    };

    let txs = || {
        vec![
            fee_paying_call_tx(0, beneficiary_predecessor_precompile(), 1),
            fee_paying_call_tx(1, beneficiary_reader_precompile(), 1),
        ]
    };
    let mut db = database();
    db.accounts.insert(state_holder(), plain_account(0, [(SLOT_0, U256::MAX)]));

    let run = |coordinate_parallel_attempt, force_sequential| {
        let trace = ReadTrace::default();
        let execution = execute_block(
            db.clone(),
            txs(),
            precompiles(trace.clone(), coordinate_parallel_attempt),
            force_sequential,
        );
        (execution, trace)
    };
    let (parallel, parallel_trace) = run(true, false);
    let (sequential, _) = run(false, true);

    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    assert!(
        parallel.2["grevm.conflict_by_miner"] >= 1,
        "the unresolved predecessor reward must be tracked as a beneficiary conflict"
    );

    let gas_used: Vec<_> = parallel
        .0
        .iter()
        .map(|outcome| match outcome {
            TxExecutionOutcome::Executed(result) => result.tx_gas_used(),
            outcome => panic!("expected successful fee-paying transaction, got {outcome:?}"),
        })
        .collect();
    assert_eq!(
        gas_used,
        [TX_BASE_GAS + PREDECESSOR_GAS, TX_BASE_GAS + READER_GAS],
        "each committed outcome must charge its custom precompile gas exactly once"
    );
    let predecessor_gas = gas_used[0];
    let total_reward: u64 = gas_used.iter().sum();
    assert_eq!(total_reward, 2 * TX_BASE_GAS + PREDECESSOR_GAS + READER_GAS);
    assert_eq!(
        final_balance(&db, &parallel.1, account::MINER_ADDRESS),
        U256::from(total_reward),
        "discarded reader incarnations must not duplicate beneficiary rewards"
    );
    assert_eq!(
        final_storage(&db, &parallel.1, state_holder(), SLOT_0),
        U256::from(predecessor_gas),
        "only the retried beneficiary observation may reach committed state"
    );

    let parallel_observations = parallel_trace.observations();
    assert_eq!(parallel_observations.first(), Some(&U256::ZERO));
    assert_eq!(parallel_observations.last(), Some(&U256::from(predecessor_gas)));
}

#[test]
fn beneficiary_storage_read_retries_after_an_earlier_precompile_write() {
    const OLD_VALUE: u64 = 17;
    const NEW_VALUE: u64 = 42;

    let precompiles = |trace: ReadTrace, coordinate_parallel_attempt: bool| {
        let writer_trace = trace.clone();
        let writer = DynParallelPrecompile::new(
            PrecompileId::Custom("grevm-test-beneficiary-storage-writer".into()),
            move |input| {
                if coordinate_parallel_attempt {
                    writer_trace.wait_for_first("the beneficiary storage read");
                }
                let reservoir = input.reservoir();
                input.state().sstore(account::MINER_ADDRESS, SLOT_0, U256::from(NEW_VALUE))?;
                Ok(successful_output(reservoir))
            },
        );
        let reader = DynParallelPrecompile::new(
            PrecompileId::Custom("grevm-test-beneficiary-storage-reader".into()),
            move |input| {
                let reservoir = input.reservoir();
                let value = input.state().sload(account::MINER_ADDRESS, SLOT_0)?.data;
                input.state().sstore(state_holder(), SLOT_1, value)?;
                trace.record(value);
                Ok(successful_output(reservoir))
            },
        );
        Arc::new(vec![
            (beneficiary_predecessor_precompile(), writer),
            (beneficiary_reader_precompile(), reader),
        ])
    };

    let mut db = database();
    db.accounts
        .get_mut(&account::MINER_ADDRESS)
        .unwrap()
        .storage
        .insert(SLOT_0, U256::from(OLD_VALUE));
    db.accounts.insert(state_holder(), plain_account(0, [(SLOT_1, U256::MAX)]));
    // A positive reward is deferred unless the transaction journal has already loaded the
    // beneficiary. This keeps the miner conflict attributable to sload's account-first path;
    // revm's zero-reward hook would load the beneficiary on its own.
    let txs = || {
        vec![
            fee_paying_call_tx(0, beneficiary_predecessor_precompile(), 1),
            fee_paying_call_tx(1, beneficiary_reader_precompile(), 1),
        ]
    };

    let run = |coordinate_parallel_attempt, force_sequential| {
        let trace = ReadTrace::default();
        let execution = execute_block(
            db.clone(),
            txs(),
            precompiles(trace.clone(), coordinate_parallel_attempt),
            force_sequential,
        );
        (execution, trace)
    };
    let (parallel, parallel_trace) = run(true, false);
    let (sequential, _) = run(false, true);

    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    assert!(
        parallel.2["grevm.conflict_by_miner"] >= 1,
        "beneficiary account coordination must cover journal-aware storage loads"
    );
    let parallel_observations = parallel_trace.observations();
    assert_eq!(parallel_observations.first(), Some(&U256::from(OLD_VALUE)));
    assert_eq!(parallel_observations.last(), Some(&U256::from(NEW_VALUE)));
    assert_eq!(
        final_storage(&db, &parallel.1, account::MINER_ADDRESS, SLOT_0),
        U256::from(NEW_VALUE)
    );
    assert_eq!(
        final_storage(&db, &parallel.1, state_holder(), SLOT_1),
        U256::from(NEW_VALUE),
        "the stale storage observation must not survive retry"
    );
}

#[test]
fn precompile_revert_and_halt_roll_back_their_writes_but_the_parent_continues() {
    const OLD_REVERT_VALUE: u64 = 17;
    const OLD_HALT_VALUE: u64 = 23;
    const TENTATIVE_REVERT_VALUE: u64 = 71;
    const TENTATIVE_HALT_VALUE: u64 = 73;

    let mut code = Vec::new();
    append_call(&mut code, reverting_precompile());
    code.extend_from_slice(&[0x15, 0x60, 0x00, 0x55]); // slot 0 = ISZERO(success)
    append_call(&mut code, halting_precompile());
    code.extend_from_slice(&[0x15, 0x60, 0x01, 0x55, 0x00]); // slot 1; STOP

    let mut db = database();
    insert_contract(&mut db, failing_precompile_caller_contract(), Bytecode::new_raw(code.into()));
    db.accounts.insert(
        state_holder(),
        plain_account(
            0,
            [(SLOT_0, U256::from(OLD_REVERT_VALUE)), (SLOT_1, U256::from(OLD_HALT_VALUE))],
        ),
    );

    let reverting = DynParallelPrecompile::new(
        PrecompileId::Custom("grevm-test-write-then-revert".into()),
        |input| {
            let reservoir = input.reservoir();
            input.state().sstore(state_holder(), SLOT_0, U256::from(TENTATIVE_REVERT_VALUE))?;
            Ok(PrecompileOutput::revert(1, Bytes::new(), reservoir))
        },
    );
    let halting = DynParallelPrecompile::new(
        PrecompileId::Custom("grevm-test-write-then-halt".into()),
        |input| {
            input.state().sstore(state_holder(), SLOT_1, U256::from(TENTATIVE_HALT_VALUE))?;
            Err(PrecompileHalt::other_static("injected halt after state write").into())
        },
    );
    let precompiles =
        Arc::new(vec![(reverting_precompile(), reverting), (halting_precompile(), halting)]);
    let txs = vec![call_tx(0, failing_precompile_caller_contract(), 1)];

    let parallel = execute_block(db.clone(), txs.clone(), precompiles.clone(), false);
    let sequential = execute_block(db.clone(), txs, precompiles, true);
    assert_eq!(parallel.0, sequential.0);
    execute::compare_bundle_state(&sequential.1, &parallel.1);
    assert_success(&parallel.0[0]);
    assert_eq!(
        final_storage(&db, &parallel.1, state_holder(), SLOT_0),
        U256::from(OLD_REVERT_VALUE),
        "a reverted precompile frame must roll back its journal writes"
    );
    assert_eq!(
        final_storage(&db, &parallel.1, state_holder(), SLOT_1),
        U256::from(OLD_HALT_VALUE),
        "a halted precompile frame must roll back its journal writes"
    );
    assert_eq!(
        final_storage(&db, &parallel.1, failing_precompile_caller_contract(), SLOT_0,),
        U256::from(1),
        "the parent must catch the precompile revert and commit its marker"
    );
    assert_eq!(
        final_storage(&db, &parallel.1, failing_precompile_caller_contract(), SLOT_1,),
        U256::from(1),
        "the parent must catch the precompile halt and commit its marker"
    );
}

#[test]
fn fatal_precompile_discards_its_parallel_incarnation_writes() {
    const OLD_VALUE: u64 = 29;
    const TENTATIVE_VALUE: u64 = 97;

    let mut db = database();
    db.accounts.insert(state_holder(), plain_account(0, [(SLOT_0, U256::from(OLD_VALUE))]));
    let precompile = DynParallelPrecompile::new(
        PrecompileId::Custom("grevm-test-write-then-fatal".into()),
        |input| {
            input.state().sstore(state_holder(), SLOT_0, U256::from(TENTATIVE_VALUE))?;
            Err(PrecompileError::Fatal("injected fatal after state write".into()).into())
        },
    );
    let scheduler = Scheduler::new_with_runtime_config(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv { beneficiary: account::MINER_ADDRESS, ..Default::default() },
        Arc::new(vec![call_tx(0, fatal_precompile(), 1)]),
        ParallelState::new(Arc::new(db.clone()), true, true),
        Some(Arc::new(vec![(fatal_precompile(), precompile)])),
        GrevmConfig {
            concurrency_level: 2,
            force_sequential: false,
            min_parallel_txs: 0,
            delegated_safety: DelegatedSafetyConfig::disabled(),
        },
    );

    let error = scheduler.execute().expect_err("the fatal precompile must abort execution");
    assert_eq!(error.txid, 0);
    assert!(
        matches!(&error.error, EVMError::Custom(message) if message.contains("injected fatal after state write")),
        "unexpected fatal precompile error: {error:?}"
    );

    let (outcomes, mut state) = scheduler.take_result_and_state();
    assert!(outcomes.is_empty(), "a fatal tx cannot enter the committed outcome prefix");
    let bundle = state.parallel_take_bundle(BundleRetention::Reverts);
    assert_eq!(
        final_storage(&db, &bundle, state_holder(), SLOT_0),
        U256::from(OLD_VALUE),
        "the fatal incarnation's journal write must not be published"
    );
}
