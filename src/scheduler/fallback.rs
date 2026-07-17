//! Sequential suffix replay used for configured and recovery fallbacks.

use super::{Scheduler, executor::build_safety_evm};
use crate::{
    GrevmError, SkipReason, TxExecutionOutcome, TxId,
    async_commit::CommitGuard,
    delegated_safety::{GravityHandler, RewardMode},
};
use alloy_evm::{EthEvm, Evm, precompiles::PrecompilesMap};
use revm::{
    Context, DatabaseCommit, DatabaseRef, ExecuteEvm, MainBuilder, MainContext,
    handler::Handler,
    precompile::{PrecompileSpecId, Precompiles},
};
use revm_context::{
    ContextSetters, ContextTr, TxEnv,
    result::{EVMError, ExecutionResult},
};
use revm_inspector::NoOpInspector;
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
        let suffix = if self.config.delegated_safety.enabled {
            let mut evm = build_safety_evm(
                state,
                self.cfg.clone(),
                self.env.clone(),
                self.custom_precompiles.as_ref(),
            );
            self.execute_sequential_suffix(committed, |txid, tx| {
                evm.ctx.set_tx(tx.clone());
                let output = GravityHandler::new(
                    txid,
                    self.config.delegated_safety.clone(),
                    self.txs.as_slice(),
                    &self.env,
                    &self.reserve_plan,
                    RewardMode::Immediate,
                )
                .run(&mut evm);
                let state = evm.finalize();
                output.inspect(|_| {
                    evm.db_mut().commit(state);
                })
            })?
        } else {
            let evm = Context::mainnet()
                .with_db(state)
                .with_cfg(self.cfg.clone())
                .with_block(self.env.clone())
                .build_mainnet_with_inspector(NoOpInspector {})
                .with_precompiles(PrecompilesMap::from_static(Precompiles::new(
                    PrecompileSpecId::from_spec_id(self.cfg.spec),
                )));
            let mut evm = EthEvm::new(evm, false);
            for (address, precompile) in self.custom_precompiles.iter() {
                let precompile = precompile.clone();
                evm.precompiles_mut().apply_precompile(address, move |_| Some(precompile));
            }
            self.execute_sequential_suffix(committed, |_, tx| {
                evm.transact_raw(tx.clone()).map(|result| {
                    evm.db_mut().commit(result.state);
                    result.result
                })
            })?
        };
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
                Err(error) => {
                    let Some(reason) = SkipReason::from_evm_error(&error) else {
                        return Err(GrevmError { txid, error });
                    };
                    tracing::error!(
                        target: "grevm::scheduler",
                        block_number = %self.env.number,
                        txid,
                        ?reason,
                        "skipping invalid transaction during sequential fallback",
                    );
                    TxExecutionOutcome::Skipped(reason)
                }
            };
            outcomes.push(outcome);
            self.metrics.execution_cnt.fetch_add(1, Ordering::Relaxed);
        }
        Ok(outcomes)
    }
}
