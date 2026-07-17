use super::*;
use revm_database::EmptyDB;
use revm_primitives::{TxKind, U256, hardfork::SpecId};
use revm_state::AccountInfo;

fn empty_scheduler(num_txs: usize) -> Scheduler<EmptyDB> {
    Scheduler::new(
        CfgEnv::new_with_spec(SpecId::SHANGHAI),
        BlockEnv::default(),
        Arc::new(vec![TxEnv::default(); num_txs]),
        ParallelState::new(EmptyDB::default(), true, false),
        false,
        None,
    )
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

    let error = scheduler.post_execute().expect_err("fatal abort must be returned");
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

    let error = scheduler.post_execute().expect_err("commit abort must be returned");
    assert_eq!(error.txid, 1);
    assert!(matches!(
        error.error,
        EVMError::Custom(message) if message == "commit error"
    ));
    assert!(scheduler.tx_results.iter().all(|result| result.lock().is_none()));
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
        false,
        None,
    );
    scheduler.results.lock().push(TxExecutionOutcome::Skipped(SkipReason::NonceTooLow));
    scheduler
        .abort(AbortReason::ParallelError { txid: 1, message: "test parallel invariant failure" });

    scheduler.post_execute().expect("sequential suffix replay must succeed");
    let results = scheduler.results.lock();
    assert_eq!(results.len(), 2);
    assert!(matches!(results[1], TxExecutionOutcome::Executed(_)));
}
