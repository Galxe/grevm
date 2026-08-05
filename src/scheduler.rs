//! Parallel Block-STM orchestration.
//!
//! Workers speculatively execute and validate transactions. A dedicated finality loop publishes a
//! contiguous, freshly validated prefix, and ordered commit applies that prefix before publishing
//! the committed boundary. Dependency edges only prioritize work; read-set validation preserves
//! correctness. If parallel execution cannot continue, the uncommitted suffix is replayed
//! sequentially from the committed state.

mod context;
mod control;
mod cursor;
mod executor;
mod fallback;
mod metrics;
mod ordered_commit;
#[cfg(test)]
mod tests;
mod wait;

use crate::{
    AbortReason, GrevmConfig, GrevmError, LocationAndType, MVMemory, ParallelState, ReadVersion,
    Task, TransactionResult, TransactionStatus, TxExecutionOutcome, TxId, TxState, TxVersion,
    beneficiary::Beneficiary,
    delegated_safety::ReservePlanner,
    incarnation_db::{IncarnationAccesses, IncarnationDb},
    tx_dependency::TxDependency,
};
use ahash::AHashSet as HashSet;
use context::SchedulerContext;
use executor::{GrevmExecutor, IncarnationExecution, ParallelTransactionExecutor};
use metrics::ExecuteMetricsCollector;
use ordered_commit::{CommitOutcome, CommittedPrefixEnd, OrderedCommitOutput, OrderedCommitter};
use parking_lot::{Mutex, MutexGuard};
use revm::DatabaseRef;
use revm_context::{BlockEnv, CfgEnv, TxEnv, result::EVMError};
use revm_primitives::Address;

use std::{
    cmp::max,
    fmt::Debug,
    panic::resume_unwind,
    sync::{Arc, OnceLock, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};
use wait::WaitSlot;

pub(crate) use cursor::PublishedCursorReader;

const STALL_TIMEOUT: Duration = Duration::from_secs(8);

struct CommitLoopResult<DBError> {
    committed: OrderedCommitOutput,
    error: Option<GrevmError<DBError>>,
}

/// Coordinates speculative execution, validation, finality, and ordered commit for one block.
///
/// # Type Parameters
/// - `DB`: A type that implements the `DatabaseRef` trait, representing the database used for
///   transaction execution.
pub struct Scheduler<DB>
where
    DB: DatabaseRef,
{
    cfg: CfgEnv,
    env: BlockEnv,
    block_size: usize,
    txs: Arc<Vec<TxEnv>>,
    state: Mutex<ParallelState<DB>>,
    results: Mutex<Vec<TxExecutionOutcome>>,
    tx_states: Vec<Mutex<TxState>>,
    tx_results: Vec<Mutex<Option<TransactionResult<DB::Error>>>>,
    tx_dependency: TxDependency,

    mv_memory: MVMemory,
    scheduler_ctx: SchedulerContext,
    /// Capability-restricted, block-scoped precompiles supplied under the retry-safety contract
    /// documented on [`Scheduler::new`].
    custom_precompiles: Arc<Vec<(Address, crate::DynParallelPrecompile)>>,
    config: GrevmConfig,
    reserve_planner: Option<Arc<ReservePlanner>>,

    started: AtomicBool,
    abort: AtomicBool,
    abort_reason: OnceLock<AbortReason<DB::Error>>,
    finality_wait: WaitSlot,
    commit_wait: WaitSlot,
    metrics: ExecuteMetricsCollector,
}

/// Cancels the other scheduler roles if the guarded thread starts unwinding.
///
/// Expected execution failures use [`AbortReason`] and return normally. This guard is only for
/// unexpected panics, whose original payload continues unwinding after peers have been released.
struct CancelOnPanic<'a, DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    scheduler: &'a Scheduler<DB>,
}

impl<DB> Drop for CancelOnPanic<'_, DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    fn drop(&mut self) {
        if thread::panicking() {
            self.scheduler.cancel();
        }
    }
}

impl<DB> Debug for Scheduler<DB>
where
    DB: DatabaseRef,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("cfg", &self.cfg)
            .field("env", &self.env)
            .field("block_size", &self.block_size)
            .field("txs", &self.txs)
            .finish()
    }
}

impl<DB> Scheduler<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    /// Create a scheduler using the environment-based runtime configuration.
    ///
    /// # Custom precompile contract
    ///
    /// Every entry in `custom_precompiles` must be safe to invoke concurrently and more than once
    /// for the same transaction. Speculative executions can be discarded and retried, while
    /// [`crate::DynParallelPrecompile`] clones share the same underlying implementation.
    /// Consequently, a custom precompile must not make non-journaled, consensus-observable
    /// mutations whose effects survive a discarded attempt or affect a later call's output, gas,
    /// status, authorization, or accounting. The restricted input exposes consensus-visible state
    /// only through journal-aware operations, so account lifecycle flags, read-your-writes,
    /// conflict tracking, and rollback remain visible to Grevm. Read-only access to immutable
    /// block-scoped data is also safe. A precompile must not keep mutable consensus state in its
    /// shared closure or mutate account/storage state through any out-of-band handle.
    ///
    /// This is an integration invariant and is not enforced at runtime. Custom precompiles that do
    /// not satisfy it must not be supplied to the parallel scheduler.
    pub fn new(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        custom_precompiles: Option<Arc<Vec<(Address, crate::DynParallelPrecompile)>>>,
    ) -> Self {
        Self::new_with_runtime_config(
            cfg,
            env,
            txs,
            state,
            custom_precompiles,
            GrevmConfig::from_env(),
        )
    }

    /// Create a scheduler with an explicit, block-scoped Grevm runtime configuration.
    ///
    /// `custom_precompiles` is subject to the retry-safety contract documented on [`Self::new`].
    ///
    /// # Panics
    ///
    /// Panics if [`GrevmConfig::concurrency_level`] is zero.
    pub fn new_with_runtime_config(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        custom_precompiles: Option<Arc<Vec<(Address, crate::DynParallelPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        assert!(config.concurrency_level > 0, "grevm concurrency level must be greater than zero");
        Self::build(cfg, env, txs, state, custom_precompiles, config)
    }

    fn build(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        custom_precompiles: Option<Arc<Vec<(Address, crate::DynParallelPrecompile)>>>,
        mut config: GrevmConfig,
    ) -> Self {
        let num_txs = txs.len();
        // The configuration may be shared across historical and current blocks. EIP-7702 safety
        // policies become effective only once the selected EVM spec activates Prague.
        config.delegated_safety = config.delegated_safety.for_spec(cfg.spec);
        // Reserve-planner construction is O(1): sender indexing and per-account maximum-cost
        // suffixes remain lazy until surviving delegated execution actually debits an account.
        let reserve_planner = config
            .delegated_safety
            .reserve_delegated_balance
            .then(|| Arc::new(ReservePlanner::new(txs.clone())));
        Self {
            cfg,
            env,
            block_size: num_txs,
            txs,
            state: Mutex::new(state),
            results: Mutex::new(vec![]),
            tx_states: (0..num_txs).map(|_| Mutex::new(TxState::default())).collect(),
            tx_results: (0..num_txs).map(|_| Mutex::new(None)).collect(),
            tx_dependency: TxDependency::new(num_txs),
            mv_memory: MVMemory::default(),
            scheduler_ctx: SchedulerContext::new(num_txs),
            custom_precompiles: custom_precompiles.unwrap_or_else(|| Arc::new(Vec::new())),
            config,
            reserve_planner,
            started: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            abort_reason: OnceLock::new(),
            finality_wait: WaitSlot::new(),
            commit_wait: WaitSlot::new(),
            metrics: ExecuteMetricsCollector::default(),
        }
    }

    /// Advance the exclusive end of the contiguous stable prefix.
    ///
    /// A transaction becomes final only while it is `Unconfirmed` and its validation timestamp is
    /// newer than every validation rewind affecting this prefix. Finality never skips an index.
    fn run_finality_loop(&self) {
        self.finality_wait.register_current_thread();
        let mut last_progress = Instant::now();
        let mut finality_idx = 0;
        let mut lower_ts = 0;
        let dependency_distance = self.metrics.dependency_distance_histogram();
        while !self.is_aborted() && finality_idx < self.block_size {
            let previous_finality_idx = finality_idx;
            while let Some((mut tx_state, effective_lower_ts)) =
                self.lock_finality_candidate(finality_idx, lower_ts)
            {
                lower_ts = effective_lower_ts;
                let incarnation = tx_state.incarnation;
                let dependency = tx_state.dependency;
                tx_state.status = TransactionStatus::Finality;
                drop(tx_state);

                let next_finality_idx = finality_idx + 1;
                self.scheduler_ctx.publish_finality(next_finality_idx);
                if finality_idx == previous_finality_idx {
                    // Start commit as soon as the first transaction in this batch is visible.
                    self.commit_wait.notify();
                }

                self.metrics.record_finalized(incarnation, dependency.is_some());
                if let Some(dep_id) = dependency {
                    dependency_distance.record((finality_idx - dep_id) as f64);
                }
                finality_idx = next_finality_idx;
            }
            let progressed = finality_idx > previous_finality_idx;
            if progressed {
                last_progress = Instant::now();
                if finality_idx - previous_finality_idx > 1 {
                    // Commit may have caught the first notification while this batch was still
                    // publishing. Wake it once more for the completed suffix.
                    self.commit_wait.notify();
                }
                thread::yield_now();
            } else {
                self.finality_wait.wait_while(STALL_TIMEOUT, || {
                    !self.is_aborted() &&
                        self.lock_finality_candidate(finality_idx, lower_ts).is_none()
                });
            }

            if last_progress.elapsed() > STALL_TIMEOUT {
                last_progress = Instant::now();
                tracing::warn!(
                    target: "grevm::scheduler",
                    block_number = %self.env.number,
                    finality_idx = self.scheduler_ctx.finality_idx(),
                    validation_idx = self.scheduler_ctx.validation_idx(),
                    execution_idx = self.scheduler_ctx.execution_frontier(),
                    "parallel execution stuck",
                );
            }
        }
    }

    fn lock_finality_candidate(
        &self,
        finality_idx: usize,
        lower_ts: usize,
    ) -> Option<(MutexGuard<'_, TxState>, usize)> {
        if finality_idx >= self.block_size || finality_idx >= self.scheduler_ctx.validation_idx() {
            return None;
        }
        // Read the validation frontier first, then decide status and timestamp eligibility under
        // the transaction lock. Together with contiguous finality, this prevents a candidate from
        // passing a rewind that invalidates it or an earlier transaction.
        let tx_state = self.tx_states[finality_idx].lock();
        if tx_state.status != TransactionStatus::Unconfirmed {
            return None;
        }

        // Carry the largest rewind timestamp through the contiguous prefix: every later candidate
        // must have been validated after that rewind as well.
        let effective_lower_ts = max(lower_ts, self.scheduler_ctx.lower_timestamp(finality_idx));
        (self.scheduler_ctx.unconfirmed_timestamp(finality_idx) > effective_lower_ts)
            .then_some((tx_state, effective_lower_ts))
    }

    /// Commit finalized transactions strictly in block order.
    ///
    /// `commit_idx`, the ordered output end, and the published committed cursor describe the same
    /// exclusive prefix. `OrderedCommitter::commit` applies state, beneficiary rewards, and the
    /// outcome before this loop publishes that prefix.
    fn run_commit_loop(&self, committer: &mut OrderedCommitter<DB>) -> CommitLoopResult<DB::Error> {
        self.commit_wait.register_current_thread();
        let mut output = OrderedCommitOutput::with_capacity(self.block_size);
        let mut commit_idx = 0;
        while !self.is_aborted() && commit_idx < self.block_size {
            let previous_commit_idx = commit_idx;
            while commit_idx < self.scheduler_ctx.finality_idx() {
                let Some(tx_result) = self.tx_results[commit_idx].lock().take() else {
                    self.abort(AbortReason::ParallelError {
                        txid: commit_idx,
                        message: "finalized transaction has no execution result",
                    });
                    return CommitLoopResult { committed: output, error: None };
                };
                let Ok(result) = tx_result.execute_result else {
                    // A transaction with an EVM error must never reach finality. This is a
                    // parallel scheduler inconsistency, so replay it from the committed state
                    // instead of trusting the speculative result.
                    self.abort(AbortReason::ParallelError {
                        txid: commit_idx,
                        message: "failed transaction reached commit",
                    });
                    return CommitLoopResult { committed: output, error: None };
                };
                let commit_start = Instant::now();
                let outcome =
                    committer.commit(commit_idx, &self.txs[commit_idx], result, &mut output);
                self.metrics.record_commit_time(commit_start.elapsed());
                match outcome {
                    Ok(CommitOutcome::Committed(committed)) => {
                        let next_commit_idx = committed.index();
                        self.scheduler_ctx.publish_commit(next_commit_idx);
                        // Publish committed state before releasing work that may require it.
                        self.tx_dependency.commit(commit_idx);
                        commit_idx = next_commit_idx;
                    }
                    Ok(CommitOutcome::NeedsSequentialFallback) => {
                        // The problematic transaction remains uncommitted. Keep the cursor at its
                        // index so sequential fallback revalidates it before processing the suffix.
                        self.abort(AbortReason::FallbackSequential);
                        return CommitLoopResult { committed: output, error: None };
                    }
                    Err(error) => {
                        // Wake every scheduler thread immediately, while also returning the exact
                        // txid and database error directly to the scoped-thread caller.
                        self.abort(AbortReason::CommitError(error.clone()));
                        return CommitLoopResult { committed: output, error: Some(error) };
                    }
                }
            }
            if commit_idx > previous_commit_idx {
                thread::yield_now();
            } else {
                self.commit_wait.wait_while(STALL_TIMEOUT, || {
                    !self.is_aborted() && commit_idx >= self.scheduler_ctx.finality_idx()
                });
            }
        }
        CommitLoopResult { committed: output, error: None }
    }

    fn install_commit_loop_result(
        &self,
        result: CommitLoopResult<DB::Error>,
    ) -> Result<CommittedPrefixEnd, GrevmError<DB::Error>> {
        // Preserve the successfully committed prefix even when the commit loop ended with an
        // error; sequential recovery and callers must observe the same state/outcome boundary.
        let CommitLoopResult { committed: output, error } = result;
        let committed = output.end();
        assert_eq!(
            committed.index(),
            self.scheduler_ctx.committed_idx(),
            "ordered output and published commit cursor must describe the same prefix",
        );
        let mut results = self.results.lock();
        assert!(results.is_empty(), "ordered commit outcomes may only be installed once");
        *results = output.into_outcomes();
        drop(results);
        error.map_or(Ok(committed), Err)
    }

    fn cancel_on_panic(&self) -> CancelOnPanic<'_, DB> {
        CancelOnPanic { scheduler: self }
    }

    fn parallel_execute_inner(
        &self,
        concurrency_level: usize,
        start_time: Instant,
    ) -> Result<(), GrevmError<DB::Error>> {
        if self.config.force_sequential || self.block_size < self.config.min_parallel_txs {
            return self.replay_uncommitted_suffix(CommittedPrefixEnd::ZERO);
        }
        let commit_thread_result = {
            // Spawn `concurrency_level` speculative workers plus one finality coordinator and one
            // ordered commit thread. Workers and commit share the concurrent cache/database view;
            // transition aggregation remains exclusively owned by commit. The scoped join ends
            // every borrow before sequential recovery can access the state again.
            let mut state = self.state.lock();
            let (state_view, commit_state) = state.split_for_parallel();
            let beneficiary_anchor = state_view
                .basic_ref(self.env.beneficiary)
                .map_err(|e| GrevmError { txid: 0, error: EVMError::Database(e) })?;
            let beneficiary =
                Beneficiary::new(self.env.beneficiary, beneficiary_anchor, self.block_size);
            let mut committer = OrderedCommitter::new(
                self.env.beneficiary,
                commit_state,
                self.cfg.disable_nonce_check,
            );
            thread::scope(|scope| {
                // If spawning or joining itself panics, cancel children before `scope` waits for
                // them. Each child has the same guard for panics in its scheduler role.
                let _scope_cancel = self.cancel_on_panic();
                let finality_thread = scope.spawn(|| {
                    let _cancel = self.cancel_on_panic();
                    self.run_finality_loop();
                    self.metrics.record_execution_time(start_time.elapsed());
                });
                let commit_thread = scope.spawn(|| {
                    let _cancel = self.cancel_on_panic();
                    self.run_commit_loop(&mut committer)
                });
                let mut workers = Vec::with_capacity(concurrency_level);
                for _ in 0..concurrency_level {
                    workers.push(scope.spawn(|| {
                        let _cancel = self.cancel_on_panic();
                        let incarnation_db =
                            IncarnationDb::new(&state_view, &self.mv_memory, &beneficiary);
                        let mut cfg = self.cfg.clone();
                        // Disable nonce checks during speculative execution. The commit thread
                        // checks the nonce against committed state; a mismatch leaves the
                        // transaction uncommitted and triggers sequential revalidation from that
                        // transaction.
                        cfg.disable_nonce_check = true;
                        let mut executor = GrevmExecutor::new(
                            incarnation_db,
                            cfg,
                            self.env.clone(),
                            self.custom_precompiles.as_ref(),
                            self.config.delegated_safety,
                            self.reserve_planner.clone(),
                        );
                        self.run_worker(&mut executor, &beneficiary);
                    }));
                }

                // Join every role explicitly. `thread::scope` otherwise replaces an automatically
                // joined child's payload with a generic "scoped thread panicked" panic.
                let mut thread_panic = None;
                let commit_result = match commit_thread.join() {
                    Ok(result) => Some(result),
                    Err(panic) => {
                        thread_panic = Some(panic);
                        None
                    }
                };
                if let Err(panic) = finality_thread.join() &&
                    thread_panic.is_none()
                {
                    thread_panic = Some(panic);
                }
                for worker in workers {
                    if let Err(panic) = worker.join() &&
                        thread_panic.is_none()
                    {
                        thread_panic = Some(panic);
                    }
                }
                if let Some(panic) = thread_panic {
                    resume_unwind(panic);
                }
                commit_result.expect("commit result exists when no scheduler thread panicked")
            })
        };
        let committed = self.install_commit_loop_result(commit_thread_result)?;
        // Recover or replay from the authoritative committed boundary recorded above.
        self.post_execute(committed)?;
        Ok(())
    }

    /// Run execution and validation tasks until the scheduler finishes or aborts.
    ///
    /// Estimate reads are rescheduled. EVM errors wait for an unresolved predecessor when
    /// possible, or trigger fallback/abort at the commit head.
    fn run_worker<WorkerDB>(
        &self,
        executor: &mut impl ParallelTransactionExecutor<WorkerDB>,
        beneficiary: &Beneficiary,
    ) where
        WorkerDB: DatabaseRef<Error = DB::Error>,
    {
        let mut task = self.next();
        while let Some(current_task) = task {
            task = match current_task {
                Task::Execution(version) => self.execute_task(executor, beneficiary, version),
                Task::Validation(version) => self.validate(beneficiary, version),
            };
            if task.is_none() && !self.is_aborted() {
                task = self.next();
            }
        }
    }

    fn execute_task<WorkerDB>(
        &self,
        executor: &mut impl ParallelTransactionExecutor<WorkerDB>,
        beneficiary: &Beneficiary,
        tx_version: TxVersion,
    ) -> Option<Task>
    where
        WorkerDB: DatabaseRef<Error = DB::Error>,
    {
        let TxVersion { txid, incarnation } = tx_version.clone();
        let mut tx_state = self.tx_states[txid].lock();
        // Cursor claims are advisory and may become stale after a rewind. The locked status and
        // incarnation are the authority for whether this task may execute.
        if tx_state.status != TransactionStatus::Executing {
            return None;
        }
        if tx_state.incarnation != incarnation {
            self.abort(AbortReason::ParallelError {
                txid,
                message: "inconsistent incarnation during execution",
            });
            return None;
        }
        self.metrics.record_execution_attempt();

        let tx_env = self.txs[txid].clone();
        let IncarnationExecution { result, accesses } =
            executor.execute_incarnation(tx_version.clone(), tx_env);

        // If this incarnation expands its write set, already validated suffix transactions may
        // have missed a new predecessor and validation must rewind to this transaction. Existing
        // dependency/rewind coverage is sufficient when the write set does not expand.
        let mut write_new_locations = false;
        let conflict;
        let mut next = None;
        match result {
            Ok(speculative_result) => {
                conflict = accesses.is_blocked();
                let IncarnationAccesses {
                    read_set,
                    write_set,
                    blocking_txs,
                    blocked_by_beneficiary,
                } = accesses;

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_ref() {
                    for location in write_set.iter() {
                        if !last_result.write_set.contains(location) {
                            write_new_locations = true;
                            break;
                        }
                    }
                    for location in &last_result.write_set {
                        if !write_set.contains(location) &&
                            let Some(mut written_transactions) = self.mv_memory.get_mut(location)
                        {
                            written_transactions.remove(&txid);
                        }
                    }
                } else {
                    write_new_locations = true;
                }

                let history_published = if conflict {
                    beneficiary.record_estimate(&tx_version)
                } else {
                    beneficiary.record_execution(&tx_version, &speculative_result)
                };
                if !history_published {
                    self.abort(AbortReason::ParallelError {
                        txid,
                        message: "stale beneficiary history publication",
                    });
                    return None;
                }

                if conflict {
                    if blocked_by_beneficiary {
                        self.metrics.record_beneficiary_conflict();
                    } else {
                        self.metrics.record_estimate_conflict();
                    }
                    self.tx_dependency.add(txid, self.latest_unfinalized_blocker(&blocking_txs));
                } else {
                    // Clearing reverse edges may hand the immediate successor directly to this
                    // worker, avoiding a cursor round trip on a linear dependency chain.
                    next = self.tx_dependency.remove(txid, true);
                }
                *last_result = Some(TransactionResult {
                    read_set,
                    write_set,
                    execute_result: Ok(speculative_result),
                });
            }
            Err(e) => {
                debug_assert!(accesses.write_set.is_empty());
                let blocked_on_estimate = accesses.is_blocked();
                let IncarnationAccesses { blocking_txs, blocked_by_beneficiary, .. } = accesses;
                let invalid_transaction = matches!(e, EVMError::Transaction(_));
                conflict = true;
                let mut write_set = HashSet::new();

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_mut() {
                    write_set = std::mem::take(&mut last_result.write_set);
                    self.mark_mv_estimate(txid, &write_set);
                }
                if !beneficiary.record_estimate(&tx_version) {
                    self.abort(AbortReason::ParallelError {
                        txid,
                        message: "stale beneficiary estimate publication",
                    });
                    return None;
                }
                *last_result = Some(TransactionResult {
                    read_set: Default::default(),
                    write_set,
                    execute_result: Err(e),
                });

                if blocked_on_estimate {
                    if blocked_by_beneficiary {
                        self.metrics.record_beneficiary_conflict();
                    } else {
                        self.metrics.record_estimate_conflict();
                    }
                    self.tx_dependency.add(txid, self.latest_unfinalized_blocker(&blocking_txs));
                } else {
                    self.metrics.record_evm_error_conflict();
                    if self.scheduler_ctx.committed_idx() == txid {
                        if invalid_transaction {
                            self.abort(AbortReason::FallbackSequential);
                        } else {
                            self.abort(AbortReason::FatalEvmError(txid));
                        }
                    }
                    self.tx_dependency.key_tx(txid, self.scheduler_ctx.commit_cursor());
                }
            }
        }

        tx_state.status =
            if conflict { TransactionStatus::Conflict } else { TransactionStatus::Executed };
        self.scheduler_ctx.executed(txid);

        if let Some(next) = next {
            self.scheduler_ctx.rewind_validation_to(txid);
            drop(tx_state);
            return self.execution_task(next);
        }
        if conflict {
            self.scheduler_ctx.rewind_validation_to(txid + 1);
        } else {
            if write_new_locations {
                self.scheduler_ctx.rewind_validation_to(txid);
            } else {
                tx_state.status = TransactionStatus::Validating;
                return Some(Task::Validation(TxVersion::new(txid, incarnation)));
            }
        }
        None
    }

    fn validate(&self, beneficiary: &Beneficiary, tx_version: TxVersion) -> Option<Task> {
        let txid = tx_version.txid;
        let incarnation = tx_version.incarnation;
        let mut tx_state = self.tx_states[txid].lock();
        let tx_result = self.tx_results[txid].lock();
        if tx_state.status != TransactionStatus::Validating {
            return None;
        }
        if tx_state.incarnation != incarnation {
            self.abort(AbortReason::ParallelError {
                txid,
                message: "inconsistent incarnation during validation",
            });
            return None;
        }
        self.metrics.record_validation_attempt();
        let Some(result) = tx_result.as_ref() else {
            self.abort(AbortReason::ParallelError {
                txid,
                message: "transaction has no result during validation",
            });
            return None;
        };
        if result.execute_result.is_err() {
            self.abort(AbortReason::ParallelError {
                txid,
                message: "failed transaction reached validation",
            });
            return None;
        }

        // Capture the timestamp before scanning. A concurrent later rewind then has a newer lower
        // bound and prevents this validation from reaching finality.
        let ts = self.scheduler_ctx.logical_timestamp();
        // Every read must still resolve to the same latest preceding incarnation, and that write
        // must not be an estimate. A storage-origin read remains valid only when no preceding
        // multi-version write exists.
        let mut conflict = false;
        let mut dependency: Option<TxId> = None;
        for (location, version) in result.read_set.iter() {
            if let ReadVersion::Beneficiary(expected) = version {
                let validation = beneficiary.validate(txid, expected);
                if !validation.is_valid() {
                    conflict = true;
                }
                if let Some(previous_id) = validation.dependency() {
                    dependency = Some(dependency.map_or(previous_id, |d| max(d, previous_id)));
                }
                continue;
            }

            if let Some(written_transactions) = self.mv_memory.get(location) {
                if let Some((&previous_id, latest_version)) =
                    written_transactions.range(..txid).next_back()
                {
                    dependency = Some(dependency.map_or(previous_id, |d| max(d, previous_id)));
                    if latest_version.estimate {
                        conflict = true;
                    } else if let ReadVersion::MvMemory(version) = version {
                        if version.txid != previous_id ||
                            version.incarnation != latest_version.incarnation
                        {
                            conflict = true;
                        }
                    } else {
                        conflict = true;
                    }
                } else if !matches!(version, ReadVersion::Storage) {
                    conflict = true;
                }
            } else if !matches!(version, ReadVersion::Storage) {
                conflict = true;
            }
        }
        if conflict {
            self.metrics.record_version_conflict();
            // Readers must not validate against writes produced by an invalid incarnation.
            self.mark_mv_estimate(txid, &result.write_set);
            if !beneficiary.invalidate(&tx_version) {
                self.abort(AbortReason::ParallelError {
                    txid,
                    message: "stale beneficiary history validation",
                });
                return None;
            }
        }

        // update transaction status
        tx_state.status = if conflict {
            self.scheduler_ctx.rewind_validation_to(txid + 1);
            TransactionStatus::Conflict
        } else {
            self.scheduler_ctx.unconfirmed(txid, ts);
            TransactionStatus::Unconfirmed
        };
        tx_state.dependency = dependency;

        if conflict {
            // update dependency
            let dep_tx = dependency.filter(|&dep| dep >= self.scheduler_ctx.finality_idx());
            self.tx_dependency.add(txid, dep_tx);
        }
        drop(tx_result);
        drop(tx_state);
        if txid == self.scheduler_ctx.finality_idx() {
            self.finality_wait.notify();
        }
        None
    }

    fn latest_unfinalized_blocker(&self, blockers: &HashSet<TxId>) -> Option<TxId> {
        let finality_idx = self.scheduler_ctx.finality_idx();
        blockers.iter().copied().filter(|&txid| txid >= finality_idx).max()
    }

    fn mark_mv_estimate(&self, txid: TxId, write_set: &HashSet<LocationAndType>) {
        for location in write_set {
            if let Some(mut written_transactions) = self.mv_memory.get_mut(location) &&
                let Some(entry) = written_transactions.get_mut(&txid)
            {
                entry.estimate = true;
            }
        }
    }

    fn execution_task(&self, execute_id: TxId) -> Option<Task> {
        let mut tx = self.tx_states[execute_id].lock();
        match tx.status {
            TransactionStatus::Initial | TransactionStatus::Conflict => {
                tx.status = TransactionStatus::Executing;
                tx.incarnation += 1;
                Some(Task::Execution(TxVersion::new(execute_id, tx.incarnation)))
            }
            // The owning worker has claimed this transaction but may not have entered
            // `execute_task` yet. A duplicate cursor claim must not release its dependents early.
            TransactionStatus::Executing => None,
            _ => {
                drop(tx);
                self.tx_dependency.remove(execute_id, false);
                self.metrics.record_useless_dependency_update();
                None
            }
        }
    }

    fn next(&self) -> Option<Task> {
        while !self.scheduler_ctx.finished() && !self.is_aborted() {
            if !self.scheduler_ctx.should_schedule(self.tx_dependency.index()) {
                thread::yield_now();
            }

            if let Some(validation_idx) =
                self.scheduler_ctx.next_validation_idx(self.tx_dependency.index())
            {
                let mut tx = self.tx_states[validation_idx].lock();
                // Rewinds can make cursor claims duplicate or stale; state under this lock decides
                // whether a validation task still exists.
                match tx.status {
                    TransactionStatus::Executed | TransactionStatus::Unconfirmed => {
                        tx.status = TransactionStatus::Validating;
                        return Some(Task::Validation(TxVersion::new(
                            validation_idx,
                            tx.incarnation,
                        )));
                    }
                    _ => {}
                }
            }

            if let Some(execute_id) = self.tx_dependency.next() &&
                let Some(task) = self.execution_task(execute_id)
            {
                return Some(task);
            }
        }
        None
    }
}
