//! Sequential suffix replay used for configured and recovery fallbacks.

use super::{Scheduler, executor::build_evm};
use crate::{
    GrevmError, InvalidTransaction, TxExecutionOutcome, TxId,
    async_commit::CommitGuard,
    delegated_safety::{BeneficiaryMode, GrevmHandler, ReserveMode},
};
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm};
use revm_context::{
    ContextSetters, ContextTr, TxEnv,
    result::{EVMError, ExecutionResult},
};
use std::sync::atomic::Ordering;

impl<DB> Scheduler<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    pub(super) fn fallback_after_parallel_error(
        &self,
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
        self.fallback_sequential()
    }

    /// Execute the uncommitted block suffix sequentially.
    pub fn fallback_sequential(&self) -> Result<(), GrevmError<DB::Error>> {
        let mut results = self.results.lock();
        let committed = results.len();
        if committed == self.block_size {
            return Ok(());
        }

        let mut commit_guard = CommitGuard::new(&self.state);
        let state = commit_guard.state_mut();
        let evm = build_evm(
            state,
            self.cfg.clone(),
            self.env.clone(),
            self.custom_precompiles.as_ref(),
            self.config.delegated_safety.forbid_delegated_create,
        );
        let mut evm = evm;
        // The planner describes the original full block, so suffix replay must keep global TxIds.
        // `committed` must not rebase future-cost lookups.
        let suffix = self.execute_sequential_suffix(committed, |txid, tx| {
            reject_nonce_overflow(evm.db_mut(), self.cfg.disable_nonce_check, tx)?;
            evm.ctx.set_tx(tx.clone());
            let reserve_mode = ReserveMode::from_planner(txid, self.reserve_planner.as_deref());
            let output: Result<ExecutionResult, EVMError<DB::Error>> =
                GrevmHandler::new(reserve_mode, BeneficiaryMode::Immediate).run(&mut evm);
            let state = evm.finalize();
            output.inspect(|_| evm.db_mut().commit(state))
        })?;
        results.extend(suffix);
        Ok(())
    }

    fn execute_sequential_suffix(
        &self,
        start: TxId,
        mut transact: impl FnMut(TxId, &TxEnv) -> Result<ExecutionResult, EVMError<DB::Error>>,
    ) -> Result<Vec<TxExecutionOutcome>, GrevmError<DB::Error>> {
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
                Err(error) => return Err(GrevmError { txid, error }),
            };
            outcomes.push(outcome);
            self.metrics.execution_cnt.fetch_add(1, Ordering::Relaxed);
        }
        Ok(outcomes)
    }
}

fn reject_nonce_overflow<DB: DatabaseRef>(
    db: &DB,
    disable_nonce_check: bool,
    tx: &TxEnv,
) -> Result<(), EVMError<DB::Error>> {
    if !disable_nonce_check &&
        tx.nonce == u64::MAX &&
        db.basic_ref(tx.caller)?.map_or(0, |info| info.nonce) == u64::MAX
    {
        return Err(InvalidTransaction::NonceOverflowInTransaction.into());
    }
    Ok(())
}
