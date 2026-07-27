//! Sequential suffix replay used for configured and recovery fallbacks.
//!
//! Replay starts at an exclusive committed boundary. State and outcomes must represent exactly that
//! prefix; successful and invalid transactions extend it, while a fatal error preserves the
//! completed portion for the caller.

use super::{Scheduler, executor::build_evm, ordered_commit::CommittedPrefixEnd};
use crate::{
    GrevmError, InvalidTransaction, TxExecutionOutcome, TxId,
    delegated_safety::{BeneficiaryMode, GrevmHandler, ReserveMode},
};
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm};
use revm_context::{
    ContextSetters, ContextTr, TxEnv,
    result::{EVMError, ExecutionResult},
};

struct SequentialReplayOutput<DBError> {
    outcomes: Vec<TxExecutionOutcome>,
    error: Option<GrevmError<DBError>>,
}

impl<DB> Scheduler<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    pub(super) fn fallback_after_parallel_error(
        &self,
        committed: CommittedPrefixEnd,
        txid: TxId,
        message: &str,
    ) -> Result<(), GrevmError<DB::Error>> {
        tracing::error!(
            target: "grevm::scheduler",
            block_number = %self.env.number,
            txid,
            reason = message,
            "parallel execution invariant failed; falling back to sequential execution",
        );
        self.replay_uncommitted_suffix(committed)
    }

    /// Execute the uncommitted block suffix sequentially.
    ///
    /// # Errors
    ///
    /// Returns an error if this scheduler has already started, or if replay encounters a database
    /// or fatal EVM error. Invalid transactions are recorded as skipped outcomes.
    pub fn fallback_sequential(&self) -> Result<(), GrevmError<DB::Error>> {
        self.run_once(|_| self.replay_uncommitted_suffix(CommittedPrefixEnd::ZERO))
    }

    pub(super) fn replay_uncommitted_suffix(
        &self,
        committed: CommittedPrefixEnd,
    ) -> Result<(), GrevmError<DB::Error>> {
        let start = committed.index();
        let result_count = self.results.lock().len();
        // State is already committed through `start`; outcomes must name the identical prefix
        // before replay can safely append the suffix.
        if start > self.block_size || result_count != start {
            return Err(GrevmError {
                txid: start.min(self.block_size.saturating_sub(1)),
                error: EVMError::Custom(format!(
                    "committed prefix mismatch: boundary={start}, outcomes={result_count}, \
                     block_size={}",
                    self.block_size,
                )),
            });
        }
        if start == self.block_size {
            return Ok(());
        }

        let replay = {
            let mut state = self.state.lock();
            let mut evm = build_evm(
                &mut *state,
                self.cfg.clone(),
                self.env.clone(),
                self.custom_precompiles.as_ref(),
                self.config.delegated_safety.forbid_delegated_create,
            );
            // The planner describes the full block, so replay retains global TxIds rather than
            // rebasing future-cost lookups at `start`.
            self.execute_sequential_suffix(start, |txid, tx| {
                reject_nonce_overflow(evm.db_mut(), self.cfg.disable_nonce_check, tx)?;
                evm.ctx.set_tx(tx.clone());
                let reserve_mode = ReserveMode::from_planner(txid, self.reserve_planner.as_deref());
                let output: Result<ExecutionResult, EVMError<DB::Error>> =
                    GrevmHandler::new(reserve_mode, BeneficiaryMode::Immediate).run(&mut evm);
                let state = evm.finalize();
                output.inspect(|_| evm.db_mut().commit(state))
            })
        };
        let SequentialReplayOutput { outcomes, error } = replay;
        self.results.lock().extend(outcomes);
        error.map_or(Ok(()), Err)
    }

    fn execute_sequential_suffix(
        &self,
        start: TxId,
        mut transact: impl FnMut(TxId, &TxEnv) -> Result<ExecutionResult, EVMError<DB::Error>>,
    ) -> SequentialReplayOutput<DB::Error> {
        let mut outcomes = Vec::with_capacity(self.block_size - start);
        for txid in start..self.block_size {
            let outcome = match transact(txid, &self.txs[txid]) {
                Ok(result) => TxExecutionOutcome::Executed(result),
                Err(EVMError::Transaction(error)) => {
                    tracing::error!(
                        target: "grevm::scheduler",
                        block_number = %self.env.number,
                        txid,
                        ?error,
                        "skipping invalid transaction during sequential fallback",
                    );
                    TxExecutionOutcome::Skipped(error)
                }
                Err(error) => {
                    return SequentialReplayOutput {
                        outcomes,
                        error: Some(GrevmError { txid, error }),
                    };
                }
            };
            outcomes.push(outcome);
            self.metrics.record_execution_attempt();
        }
        SequentialReplayOutput { outcomes, error: None }
    }
}

fn reject_nonce_overflow<DB: DatabaseRef>(
    db: &DB,
    disable_nonce_check: bool,
    tx: &TxEnv,
) -> Result<(), EVMError<DB::Error>> {
    // revm increments the sender nonce with saturating arithmetic. Detect MAX explicitly so
    // sequential recovery preserves the protocol's nonce-overflow invalid classification.
    if !disable_nonce_check &&
        tx.nonce == u64::MAX &&
        db.basic_ref(tx.caller)?.map_or(0, |info| info.nonce) == u64::MAX
    {
        return Err(InvalidTransaction::NonceOverflowInTransaction.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParallelState;
    use revm_context::{
        BlockEnv, CfgEnv,
        result::{Output, SuccessReason},
    };
    use revm_database::EmptyDB;
    use revm_primitives::{Bytes, hardfork::SpecId};
    use std::sync::Arc;

    fn success() -> ExecutionResult {
        ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas_used: 21_000,
            gas_refunded: 0,
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        }
    }

    fn scheduler(num_txs: usize) -> Scheduler<EmptyDB> {
        Scheduler::new(
            CfgEnv::new_with_spec(SpecId::SHANGHAI),
            BlockEnv::default(),
            Arc::new(vec![TxEnv::default(); num_txs]),
            ParallelState::new(EmptyDB::default(), true, false),
            None,
        )
    }

    #[test]
    fn sequential_fatal_error_preserves_the_completed_prefix() {
        let scheduler = scheduler(3);
        let replay = scheduler.execute_sequential_suffix(0, |txid, _| {
            if txid == 1 { Err(EVMError::Custom("fatal".to_owned())) } else { Ok(success()) }
        });

        assert_eq!(replay.outcomes.len(), 1);
        let error = replay.error.expect("the second transaction must fail");
        assert_eq!(error.txid, 1);
        assert!(matches!(error.error, EVMError::Custom(message) if message == "fatal"));
    }

    #[test]
    fn skipped_transaction_still_advances_the_replay_prefix() {
        let scheduler = scheduler(2);
        let replay = scheduler.execute_sequential_suffix(0, |txid, _| {
            if txid == 0 {
                Err(EVMError::Transaction(InvalidTransaction::NonceTooLow { tx: 0, state: 1 }))
            } else {
                Ok(success())
            }
        });

        assert!(replay.error.is_none());
        assert_eq!(replay.outcomes.len(), 2);
        assert!(matches!(replay.outcomes[0], TxExecutionOutcome::Skipped(_)));
        assert!(matches!(replay.outcomes[1], TxExecutionOutcome::Executed(_)));
    }
}
