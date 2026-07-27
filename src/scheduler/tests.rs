use super::*;
#[cfg(feature = "test-utils")]
use crate::DelegatedSafetyConfig;
use crate::InvalidTransaction;
use revm_context::{
    DBErrorMarker,
    result::{ExecutionResult, Output, ResultAndState, SuccessReason},
};
use revm_database::EmptyDB;
use revm_primitives::{B256, Bytes, TxKind, U256, hardfork::SpecId};
use revm_state::{AccountInfo, Bytecode};
use std::{
    fmt::{Display, Formatter},
    sync::Barrier,
};

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommitDbError;

impl Display for CommitDbError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("commit database error")
    }
}

impl core::error::Error for CommitDbError {}
impl DBErrorMarker for CommitDbError {}

#[derive(Clone, Debug)]
struct CommitFailDb {
    coinbase: Address,
}

impl DatabaseRef for CommitFailDb {
    type Error = CommitDbError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if address == self.coinbase { Ok(None) } else { Err(CommitDbError) }
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        Err(CommitDbError)
    }

    fn storage_ref(&self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
        Err(CommitDbError)
    }

    fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
        Err(CommitDbError)
    }
}

#[derive(Clone, Debug)]
struct WorkerPanicDb {
    coinbase: Address,
}

impl DatabaseRef for WorkerPanicDb {
    type Error = CommitDbError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if address == self.coinbase {
            Ok(None)
        } else {
            panic!("injected speculative worker panic")
        }
    }

    fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(Bytecode::default())
    }

    fn storage_ref(&self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
        Ok(U256::ZERO)
    }

    fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
        Ok(B256::ZERO)
    }
}

fn empty_scheduler(num_txs: usize) -> Scheduler<EmptyDB> {
    Scheduler::new(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv::default(),
        Arc::new(vec![TxEnv::default(); num_txs]),
        ParallelState::new(EmptyDB::default(), true, false),
        None,
    )
}

#[test]
fn scheduler_returns_an_error_for_a_second_execution() {
    let scheduler = empty_scheduler(0);
    scheduler.parallel_execute(Some(1)).expect("first execution must succeed");

    let error = scheduler.parallel_execute(Some(1)).expect_err("reusing scheduler must fail");
    assert!(matches!(
        error.error,
        EVMError::Custom(message) if message.contains("can execute only once")
    ));
}

#[test]
fn speculative_worker_panic_cancels_peers_without_becoming_an_abort_reason() {
    let coinbase = Address::from([0xCB; 20]);
    let caller = Address::from([0xCA; 20]);
    let tx = TxEnv {
        caller,
        kind: TxKind::Call(caller),
        gas_limit: 21_000,
        gas_price: 1,
        ..Default::default()
    };
    let config =
        GrevmConfig { concurrency_level: 1, min_parallel_txs: 0, ..GrevmConfig::default() };
    let scheduler = Scheduler::new_with_runtime_config(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv { beneficiary: coinbase, ..Default::default() },
        Arc::new(vec![tx]),
        ParallelState::new(WorkerPanicDb { coinbase }, true, false),
        None,
        config,
    );

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scheduler.parallel_execute(Some(1))
    }))
    .expect_err("the original worker panic must reach the caller");
    assert_eq!(panic.downcast_ref::<&str>().copied(), Some("injected speculative worker panic"),);
    assert!(scheduler.is_aborted());
    assert!(
        scheduler.abort_reason.get().is_none(),
        "a panic must not masquerade as a recoverable execution error"
    );
}

#[test]
fn fallback_then_execute_is_rejected_without_replaying_results() {
    let scheduler = empty_scheduler(1);
    scheduler.fallback_sequential().expect("fallback execution must succeed");
    assert_eq!(scheduler.results.lock().len(), 1);

    let error = scheduler.execute().expect_err("execution after fallback must fail");
    assert!(matches!(
        error.error,
        EVMError::Custom(message) if message.contains("can execute only once")
    ));
    assert_eq!(scheduler.results.lock().len(), 1, "the block must not be executed twice");
}

#[test]
fn concurrent_fallback_and_execute_have_exactly_one_winner() {
    let scheduler = empty_scheduler(0);
    let start = Barrier::new(3);

    let (fallback, execute) = std::thread::scope(|scope| {
        let fallback = scope.spawn(|| {
            start.wait();
            scheduler.fallback_sequential()
        });
        let execute = scope.spawn(|| {
            start.wait();
            scheduler.execute()
        });
        start.wait();
        (fallback.join().unwrap(), execute.join().unwrap())
    });

    assert_eq!(
        usize::from(fallback.is_ok()) + usize::from(execute.is_ok()),
        1,
        "exactly one public execution entry point must claim the scheduler"
    );
    let loser = fallback.err().or_else(|| execute.err()).expect("one call must be rejected");
    assert!(matches!(
        loser.error,
        EVMError::Custom(message) if message.contains("can execute only once")
    ));
}

#[test]
fn duplicate_claim_does_not_release_dependents_before_execution_starts() {
    let scheduler = empty_scheduler(2);

    assert_eq!(scheduler.tx_dependency.next(), Some(0));
    assert!(matches!(scheduler.execution_task(0), Some(Task::Execution(_))));
    assert_eq!(scheduler.tx_dependency.next(), Some(1));

    // Requeue transaction 0 as a predecessor while its owning worker is between creating the
    // execution task and entering `execute_task`.
    scheduler.tx_dependency.add(1, Some(0));
    assert_eq!(scheduler.tx_dependency.next(), Some(0));
    assert!(scheduler.execution_task(0).is_none());
    assert_eq!(
        scheduler.tx_dependency.next(),
        None,
        "the duplicate claim must leave transaction 1 blocked"
    );

    scheduler.tx_dependency.remove(0, false);
    assert_eq!(scheduler.tx_dependency.next(), Some(1));
}

#[test]
fn finality_candidate_obeys_validation_rewinds_and_timestamps() {
    let scheduler = empty_scheduler(1);
    scheduler.scheduler_ctx.executed(0);
    assert_eq!(scheduler.scheduler_ctx.next_validation_idx(1), Some(0));

    let first_timestamp = scheduler.scheduler_ctx.logical_timestamp();
    scheduler.scheduler_ctx.unconfirmed(0, first_timestamp);
    scheduler.tx_states[0].lock().status = TransactionStatus::Unconfirmed;
    assert!(scheduler.lock_finality_candidate(0, 0).is_some());

    scheduler.scheduler_ctx.rewind_validation_to(0);
    assert!(scheduler.lock_finality_candidate(0, 0).is_none());

    assert_eq!(scheduler.scheduler_ctx.next_validation_idx(1), Some(0));
    assert!(
        scheduler.lock_finality_candidate(0, 0).is_none(),
        "the pre-rewind unconfirmed timestamp must remain ineligible"
    );

    let revalidated_timestamp = scheduler.scheduler_ctx.logical_timestamp();
    scheduler.scheduler_ctx.unconfirmed(0, revalidated_timestamp);
    assert!(scheduler.lock_finality_candidate(0, 0).is_some());
}

#[test]
fn fatal_execution_abort_uses_recorded_txid_not_finality_idx() {
    let scheduler = empty_scheduler(3);
    *scheduler.tx_results[2].lock() = Some(TransactionResult {
        read_set: Default::default(),
        write_set: Default::default(),
        execute_result: Err(EVMError::Custom("fatal execution error".to_owned())),
    });
    scheduler.abort(AbortReason::FatalEvmError(2));

    let error =
        scheduler.post_execute(CommittedPrefixEnd::ZERO).expect_err("fatal abort must be returned");
    assert_eq!(scheduler.scheduler_ctx.finality_idx(), 0);
    assert_eq!(error.txid, 2);
    assert!(matches!(
        error.error,
        EVMError::Custom(message) if message == "fatal execution error"
    ));
}

#[test]
fn commit_abort_carries_error_without_using_tx_results() {
    let scheduler = empty_scheduler(3);
    scheduler.abort(AbortReason::CommitError(GrevmError {
        txid: 1,
        error: EVMError::Custom("commit error".to_owned()),
    }));

    let error = scheduler
        .post_execute(CommittedPrefixEnd::ZERO)
        .expect_err("commit abort must be returned");
    assert_eq!(error.txid, 1);
    assert!(matches!(
        error.error,
        EVMError::Custom(message) if message == "commit error"
    ));
    assert!(scheduler.tx_results.iter().all(|result| result.lock().is_none()));
}

#[test]
fn ordered_commit_database_error_aborts_and_returns_exact_error() {
    let caller = Address::from([0xCA; 20]);
    let coinbase = Address::from([0xCB; 20]);
    let tx = TxEnv { caller, ..Default::default() };
    let scheduler = Scheduler::new(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv { beneficiary: coinbase, ..Default::default() },
        Arc::new(vec![tx]),
        ParallelState::new(CommitFailDb { coinbase }, true, false),
        None,
    );
    *scheduler.tx_results[0].lock() = Some(TransactionResult {
        read_set: Default::default(),
        write_set: Default::default(),
        execute_result: Ok(ResultAndState {
            result: ExecutionResult::Success {
                reason: SuccessReason::Stop,
                gas_used: 21_000,
                gas_refunded: 0,
                logs: Vec::new(),
                output: Output::Call(Bytes::new()),
            },
            state: Default::default(),
        }),
    });
    scheduler.scheduler_ctx.publish_finality(1);

    let mut state = scheduler.state.lock();
    let (_, commit_state) = state.split_for_parallel();
    let mut committer =
        OrderedCommitter::try_new(coinbase, SpecId::SHANGHAI, 0, commit_state, false)
            .expect("coinbase preload must succeed");

    let run = scheduler.run_commit_loop(&mut committer);
    let error = run.error.expect("commit must return DB error");

    assert_eq!(error.txid, 0);
    assert!(matches!(error.error, EVMError::Database(CommitDbError)));
    assert!(scheduler.is_aborted(), "commit error must stop every scheduler loop");
    assert_eq!(scheduler.scheduler_ctx.committed_idx(), 0);
    let Some(AbortReason::CommitError(abort_error)) = scheduler.abort_reason.get() else {
        panic!("commit error must be preserved as the abort reason");
    };
    assert_eq!(abort_error.txid, 0);
    assert!(matches!(abort_error.error, EVMError::Database(CommitDbError)));
    assert_eq!(run.committed.end(), CommittedPrefixEnd::ZERO);
}

#[test]
fn ordered_commit_error_retains_the_successful_prefix() {
    let coinbase = Address::from([0xCB; 20]);
    let failing_caller = Address::from([0xCC; 20]);
    let txs = Arc::new(vec![
        TxEnv { caller: coinbase, ..Default::default() },
        TxEnv { caller: failing_caller, ..Default::default() },
    ]);
    let scheduler = Scheduler::new(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv { beneficiary: coinbase, ..Default::default() },
        txs,
        ParallelState::new(CommitFailDb { coinbase }, true, false),
        None,
    );
    for tx_result in &scheduler.tx_results {
        *tx_result.lock() = Some(TransactionResult {
            read_set: Default::default(),
            write_set: Default::default(),
            execute_result: Ok(ResultAndState {
                result: ExecutionResult::Success {
                    reason: SuccessReason::Stop,
                    gas_used: 21_000,
                    gas_refunded: 0,
                    logs: Vec::new(),
                    output: Output::Call(Bytes::new()),
                },
                state: Default::default(),
            }),
        });
    }
    scheduler.scheduler_ctx.publish_finality(2);

    let mut state = scheduler.state.lock();
    let (_, commit_state) = state.split_for_parallel();
    let mut committer =
        OrderedCommitter::try_new(coinbase, SpecId::SHANGHAI, 0, commit_state, false)
            .expect("coinbase preload");
    let run = scheduler.run_commit_loop(&mut committer);

    assert_eq!(run.committed.end(), CommittedPrefixEnd::for_test(1));
    assert_eq!(scheduler.scheduler_ctx.committed_idx(), 1);
    let error = scheduler
        .install_commit_loop_result(run)
        .expect_err("second commit must fail after installing its prefix");
    assert_eq!(error.txid, 1);
    assert_eq!(scheduler.results.lock().len(), 1);
}

#[cfg(feature = "test-utils")]
#[test]
fn execution_metrics_are_reported_once_for_sequential_and_error_paths() {
    let sequential = Scheduler::new_with_runtime_config(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv::default(),
        Arc::new(vec![TxEnv::default()]),
        ParallelState::new(EmptyDB::default(), true, false),
        None,
        GrevmConfig {
            concurrency_level: 1,
            force_sequential: true,
            min_parallel_txs: 0,
            delegated_safety: DelegatedSafetyConfig::default(),
        },
    );
    sequential.execute().expect("configured sequential execution");
    let snapshot = sequential.metrics_snapshot();
    assert_eq!(snapshot["grevm.total_tx_cnt"], 1);
    assert!(snapshot["grevm.total_time"] > 0);
    assert_eq!(sequential.metrics.report_count(), 1);

    let explicit = empty_scheduler(1);
    explicit.fallback_sequential().expect("explicit sequential execution");
    let snapshot = explicit.metrics_snapshot();
    assert_eq!(snapshot["grevm.total_tx_cnt"], 1);
    assert!(snapshot["grevm.total_time"] > 0);
    assert_eq!(explicit.metrics.report_count(), 1);

    let beneficiary = Address::from([0xCA; 20]);
    let preload_error = Scheduler::new_with_runtime_config(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv { beneficiary, ..Default::default() },
        Arc::new(vec![TxEnv::default()]),
        ParallelState::new(CommitFailDb { coinbase: Address::ZERO }, true, false),
        None,
        GrevmConfig {
            concurrency_level: 1,
            force_sequential: false,
            min_parallel_txs: 0,
            delegated_safety: DelegatedSafetyConfig::default(),
        },
    );
    preload_error.execute().expect_err("coinbase preload must fail");
    let snapshot = preload_error.metrics_snapshot();
    assert_eq!(snapshot["grevm.total_tx_cnt"], 1);
    assert!(snapshot["grevm.total_time"] > 0);
    assert_eq!(preload_error.metrics.report_count(), 1);
}

#[test]
fn parallel_error_replays_suffix_from_committed_prefix() {
    let caller = Address::from([0xCA; 20]);
    let receiver = Address::from([0xCB; 20]);
    let state = ParallelState::new(EmptyDB::default(), true, false);
    state.insert_account(
        caller,
        AccountInfo { balance: U256::from(1_000_000), nonce: 1, ..Default::default() },
    );
    let suffix_tx = TxEnv {
        caller,
        gas_limit: 21_000,
        kind: TxKind::Call(receiver),
        value: U256::from(1),
        nonce: 1,
        ..Default::default()
    };
    let scheduler = Scheduler::new(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv::default(),
        Arc::new(vec![TxEnv::default(), suffix_tx]),
        state,
        None,
    );
    scheduler
        .results
        .lock()
        .push(TxExecutionOutcome::Skipped(InvalidTransaction::NonceTooLow { tx: 0, state: 1 }));
    scheduler
        .abort(AbortReason::ParallelError { txid: 0, message: "test parallel invariant failure" });

    scheduler
        .post_execute(CommittedPrefixEnd::for_test(1))
        .expect("sequential suffix replay must succeed");
    let results = scheduler.results.lock();
    assert_eq!(results.len(), 2);
    assert!(matches!(results[1], TxExecutionOutcome::Executed(_)));
}
