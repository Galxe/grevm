mod context;
mod executor;
mod fallback;
mod metrics;
#[cfg(test)]
mod tests;

use crate::{
    AbortReason, GrevmConfig, GrevmError, LocationAndType, MVMemory, ParallelState, ReadVersion,
    SkipReason, Task, TransactionResult, TransactionStatus, TxExecutionOutcome, TxId, TxState,
    TxVersion,
    async_commit::{CommitGuard, StateAsyncCommit},
    cache_db::CacheDB,
    delegated_safety::{DelegatedSafetyConfig, ReservePlan, ReservePlanError},
    hint::ParallelExecutionHints,
    tx_dependency::TxDependency,
};
use ::metrics::histogram;
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use alloy_evm::precompiles::DynPrecompile;
use context::SchedulerContext;
use executor::{ParallelTransactionExecutor, SafetyExecutor, StandardExecutor};
use metrics::ExecuteMetricsCollector;
use parking_lot::Mutex;
use revm::DatabaseRef;
use revm_context::{BlockEnv, CfgEnv, TxEnv, result::EVMError};
use revm_primitives::Address;

use std::{
    cell::UnsafeCell,
    cmp::max,
    fmt::Debug,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
};

/// The `Scheduler` struct is responsible for managing the parallel execution of transactions
/// in a block. It coordinates the execution, validation, and finalization of transactions
/// while handling dependencies and conflicts between them.
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
    state: UnsafeCell<ParallelState<DB>>,
    results: Mutex<Vec<TxExecutionOutcome>>,
    tx_states: Vec<Mutex<TxState>>,
    tx_results: Vec<Mutex<Option<TransactionResult<DB::Error>>>>,
    tx_dependency: TxDependency,

    mv_memory: MVMemory,
    scheduler_ctx: SchedulerContext,
    custom_precompiles: Arc<Vec<(Address, DynPrecompile)>>,
    config: GrevmConfig,
    reserve_plan: Arc<OnceLock<Result<ReservePlan, ReservePlanError>>>,

    abort: AtomicBool,
    abort_reason: OnceLock<AbortReason<DB::Error>>,
    metrics: ExecuteMetricsCollector,
}

// SAFETY: Scheduler is shared across threads via `thread::scope`. The `UnsafeCell<ParallelState>`
// is safe because: (1) only the commit thread mutates it (via StateAsyncCommit), serialized by
// finality ordering, (2) worker threads only read via DatabaseRef (DashMap, thread-safe),
// (3) fallback_sequential() is only called after all threads have joined.
unsafe impl<DB: DatabaseRef + Send + Sync> Sync for Scheduler<DB> where DB::Error: Send + Sync {}

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
    /// Create a Scheduler for parallel execution
    pub fn new(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
    ) -> Self {
        Self::new_with_config(
            cfg,
            env,
            txs,
            state,
            with_hints,
            custom_precompiles,
            GrevmConfig::from_env(),
        )
    }

    /// Create a scheduler with an explicit, block-scoped runtime configuration.
    pub fn new_with_config(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        assert!(config.concurrency_level > 0, "grevm concurrency level must be greater than zero");
        Self::build(cfg, env, txs, state, with_hints, custom_precompiles, config)
    }

    /// Compatibility constructor for callers that only override delegated-account safety.
    #[deprecated(note = "use Scheduler::new_with_config and GrevmConfig")]
    pub fn new_with_delegated_safety(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        delegated_safety: DelegatedSafetyConfig,
    ) -> Self {
        Self::new_with_config(
            cfg,
            env,
            txs,
            state,
            with_hints,
            custom_precompiles,
            GrevmConfig::from_env().with_delegated_safety(delegated_safety),
        )
    }

    fn build(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        let num_txs = txs.len();
        let tx_dependency = if with_hints {
            ParallelExecutionHints::new(txs.clone()).parse_hints()
        } else {
            TxDependency::new(num_txs)
        };
        Self {
            cfg,
            env,
            block_size: num_txs,
            txs,
            state: UnsafeCell::new(state),
            results: Mutex::new(vec![]),
            tx_states: (0..num_txs).map(|_| Mutex::new(TxState::default())).collect(),
            tx_results: (0..num_txs).map(|_| Mutex::new(None)).collect(),
            tx_dependency,
            mv_memory: MVMemory::new(),
            scheduler_ctx: SchedulerContext::new(num_txs),
            custom_precompiles: custom_precompiles.unwrap_or_else(|| Arc::new(Vec::new())),
            config,
            reserve_plan: Arc::new(OnceLock::new()),
            abort: AtomicBool::new(false),
            abort_reason: OnceLock::new(),
            metrics: ExecuteMetricsCollector::default(),
        }
    }

    fn async_finality(&self) {
        let mut start = Instant::now();
        let mut finality_idx = 0;
        let mut lower_ts = 0;
        let dependency_distance = histogram!("grevm.dependency_distance");
        while !self.abort.load(Ordering::Acquire) && finality_idx < self.block_size {
            while finality_idx < self.block_size &&
                finality_idx < self.scheduler_ctx.validation_idx()
            {
                if self.tx_states[finality_idx].lock().status != TransactionStatus::Unconfirmed {
                    break;
                }
                lower_ts = max(
                    lower_ts,
                    self.scheduler_ctx.lower_ts[finality_idx].load(Ordering::Acquire),
                );
                // Rolling back the `validation_idx` implies that the commitment time of subsequent
                // transactions must be logically later than the current timestamp.
                if self.scheduler_ctx.unconfirmed_ts[finality_idx].load(Ordering::Acquire) <=
                    lower_ts
                {
                    break;
                }
                let mut tx_state = self.tx_states[finality_idx].lock();
                if tx_state.status != TransactionStatus::Unconfirmed {
                    break;
                }
                tx_state.status = TransactionStatus::Finality;
                self.scheduler_ctx.finality_idx.fetch_add(1, Ordering::AcqRel);

                if tx_state.incarnation > 1 {
                    self.metrics.conflict_txs.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(dep_id) = tx_state.dependency {
                    dependency_distance.record((finality_idx - dep_id) as f64);
                    if tx_state.incarnation == 1 {
                        self.metrics.one_attempt_with_dependency.fetch_add(1, Ordering::Relaxed);
                    } else if tx_state.incarnation > 2 {
                        self.metrics.more_attempts_with_dependency.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    self.metrics.no_dependency_txs.fetch_add(1, Ordering::Relaxed);
                }
                finality_idx += 1;
            }
            thread::yield_now();

            if (Instant::now() - start).as_millis() > 8_000 {
                start = Instant::now();
                tracing::warn!(
                    target: "grevm::scheduler",
                    block_number = %self.env.number,
                    finality_idx = self.scheduler_ctx.finality_idx(),
                    validation_idx = self.scheduler_ctx.validation_idx(),
                    execution_idx = self.scheduler_ctx.executed_set.continuous_idx(),
                    "parallel execution stuck",
                );
            }
        }
    }

    fn async_commit(&self, commiter: &Mutex<StateAsyncCommit<DB>>) {
        let mut commit_idx = 0;
        let mut commiter = commiter.lock();
        while !self.abort.load(Ordering::Acquire) && commit_idx < self.block_size {
            while commit_idx < self.scheduler_ctx.finality_idx.load(Ordering::Acquire) {
                let Some(tx_result) = self.tx_results[commit_idx].lock().take() else {
                    self.abort(AbortReason::ParallelError {
                        txid: commit_idx,
                        message: "finalized transaction has no execution result",
                    });
                    return;
                };
                let Ok(result) = tx_result.execute_result else {
                    self.abort(AbortReason::ParallelError {
                        txid: commit_idx,
                        message: "failed transaction reached commit",
                    });
                    return;
                };
                let commit_start = Instant::now();
                let fallback = commiter.commit(commit_idx, &self.txs[commit_idx], result);
                self.metrics
                    .commit_time
                    .fetch_add(commit_start.elapsed().as_nanos() as usize, Ordering::Relaxed);
                if let Err(error) = commiter.commit_result() {
                    self.abort(AbortReason::CommitError(error.clone()));
                    return;
                }
                if fallback {
                    self.scheduler_ctx.commit_idx.fetch_add(1, Ordering::AcqRel);
                    self.tx_dependency.commit(commit_idx);
                    self.abort(AbortReason::FallbackSequential);
                    return;
                }
                self.scheduler_ctx.commit_idx.fetch_add(1, Ordering::AcqRel);
                self.tx_dependency.commit(commit_idx);
                commit_idx += 1;
            }
            thread::yield_now();
        }
    }

    /// Take transaction outcomes and `ParallelState`.
    pub fn take_result_and_state(self) -> (Vec<TxExecutionOutcome>, ParallelState<DB>) {
        (self.results.into_inner(), self.state.into_inner())
    }

    /// Execute using the scheduler's unified runtime configuration.
    pub fn execute(&self) -> Result<(), GrevmError<DB::Error>> {
        self.parallel_execute(None)
    }

    /// Execute with an optional per-call concurrency override.
    ///
    /// New integrations should configure [`GrevmConfig::concurrency_level`] and call
    /// [`Self::execute`].
    pub fn parallel_execute(
        &self,
        concurrency_level: Option<usize>,
    ) -> Result<(), GrevmError<DB::Error>> {
        let start_time = Instant::now();
        self.metrics.total_tx_cnt.store(self.block_size, Ordering::Relaxed);
        let concurrency_level = concurrency_level.unwrap_or(self.config.concurrency_level);
        assert!(concurrency_level > 0, "grevm concurrency level must be greater than zero");
        if self.config.force_sequential || self.block_size < self.config.min_parallel_txs {
            return self.fallback_sequential();
        }
        let commiter = Mutex::new(StateAsyncCommit::new(
            self.env.beneficiary,
            self.cfg.spec,
            self.env.basefee,
            CommitGuard::new(&self.state),
            self.cfg.disable_nonce_check,
        ));

        let state_ref = unsafe { &*self.state.get() };
        commiter.lock().init().map_err(|e| GrevmError { txid: 0, error: EVMError::Database(e) })?;
        thread::scope(|scope| {
            scope.spawn(|| {
                self.async_finality();
                self.metrics
                    .execution_time
                    .store(start_time.elapsed().as_nanos() as usize, Ordering::Relaxed);
            });
            scope.spawn(|| {
                self.async_commit(&commiter);
            });
            for _ in 0..concurrency_level {
                scope.spawn(|| {
                    let cache_db = CacheDB::new(
                        self.cfg.spec,
                        self.env.beneficiary,
                        state_ref,
                        &self.mv_memory,
                        &self.scheduler_ctx.commit_idx,
                    );
                    let mut cfg = self.cfg.clone();
                    // Disable nonce check to bypass the EVM's strict sequential nonce verification;
                    // the nonce is re-checked when the transaction commits.
                    cfg.disable_nonce_check = true;
                    if self.config.delegated_safety.enabled {
                        let mut executor = SafetyExecutor::new(
                            cache_db,
                            cfg,
                            self.env.clone(),
                            self.custom_precompiles.as_ref(),
                            self.config.delegated_safety.clone(),
                            self.txs.clone(),
                            self.reserve_plan.clone(),
                        );
                        self.run_worker(&mut executor);
                        return;
                    }
                    let mut executor = StandardExecutor::new(
                        cache_db,
                        cfg,
                        self.env.clone(),
                        self.custom_precompiles.as_ref(),
                    );
                    self.run_worker(&mut executor);
                });
            }
        });
        {
            let mut commiter = commiter.lock();
            // Return fatal commit errors. Recoverable nonce errors request sequential fallback
            // without populating `commit_result`.
            if let Err(e) = commiter.commit_result() {
                return Err(e.clone());
            }
            self.results.lock().extend(commiter.take_result());
        }
        // Return error if execution failed
        self.post_execute()?;
        self.metrics.reset_validation_idx_cnt.store(
            self.scheduler_ctx.reset_validation_idx_cnt.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.metrics.total_time.store(start_time.elapsed().as_nanos() as usize, Ordering::Relaxed);
        self.metrics.report();
        Ok(())
    }

    fn post_execute(&self) -> Result<(), GrevmError<DB::Error>> {
        if self.abort.load(Ordering::Acquire) {
            match self.abort_reason.get() {
                Some(AbortReason::FatalEvmError(txid)) => {
                    let error = self.tx_results.get(*txid).and_then(|result| {
                        result
                            .lock()
                            .as_ref()
                            .and_then(|result| result.execute_result.as_ref().err().cloned())
                    });
                    if let Some(error) = error {
                        return Err(GrevmError { txid: *txid, error });
                    }

                    // Losing the execution error is itself a parallel scheduler inconsistency.
                    // The committed prefix remains authoritative, so replay the suffix.
                    return self.fallback_after_parallel_error(
                        *txid,
                        "fatal execution abort has no matching transaction error",
                    );
                }
                // `parallel_execute` normally returns `commit_result` before reaching this branch.
                // Keeping the exact error here makes `post_execute` correct on its own as well.
                Some(AbortReason::CommitError(error)) => return Err(error.clone()),
                Some(AbortReason::ParallelError { txid, message }) => {
                    return self.fallback_after_parallel_error(*txid, message);
                }
                // Grevm maintains full compatibility with self-destruct operations while
                // preserving the ability to fall back to sequential execution when necessary.
                // Although this code path remains theoretically unreachable in normal
                // operation, we deliberately retain it as a safeguard. Notably, Grevm
                // implements an optimized rollback mechanism - when parallel execution fails,
                // the system can resume sequential processing from the problematic transaction
                // rather than restarting the entire block. This represents a significant
                // optimization for rare edge cases, effectively preventing severe performance
                // degradation that could otherwise drastically slow down parallel execution
                // throughput.
                Some(AbortReason::SelfDestructed | AbortReason::FallbackSequential) => {
                    return self.fallback_sequential();
                }
                None => {
                    return self.fallback_after_parallel_error(
                        self.scheduler_ctx.commit_idx.load(Ordering::Acquire),
                        "parallel execution aborted without a reason",
                    );
                }
            }
        }
        Ok(())
    }

    fn abort(&self, abort_reason: AbortReason<DB::Error>) {
        self.abort_reason.get_or_init(|| abort_reason);
        self.abort.store(true, Ordering::Release);
    }

    /// After execution, transactions are marked as conflict status in three scenarios:
    /// ​- EVM Execution Failure: The transaction fails during EVM processing
    /// - ​Read Estimate Data: The transaction accesses uncommitted state estimates
    /// - ​Unconfirmed Miner/Self-Destruct Accounts: The transaction interacts with miner rewards or
    ///   self-destructed accounts before their committing transaction is finalized (txid ≠
    ///   commit_idx)
    fn run_worker<'db>(&self, executor: &mut impl ParallelTransactionExecutor<'db, DB>)
    where
        DB: 'db,
    {
        let mut task = self.next();
        while let Some(current_task) = task {
            task = match current_task {
                Task::Execution(version) => self.execute_task(executor, version),
                Task::Validation(version) => self.validate(version),
            };
            if task.is_none() && !self.abort.load(Ordering::Acquire) {
                task = self.next();
            }
        }
    }

    fn execute_task<'db>(
        &self,
        executor: &mut impl ParallelTransactionExecutor<'db, DB>,
        tx_version: TxVersion,
    ) -> Option<Task>
    where
        DB: 'db,
    {
        let TxVersion { txid, incarnation } = tx_version.clone();
        let mut tx_state = self.tx_states[txid].lock();
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
        self.metrics.execution_cnt.fetch_add(1, Ordering::Relaxed);

        let tx_env = self.txs[txid].clone();
        let commit_idx = self.scheduler_ctx.commit_idx.load(Ordering::Acquire);
        let result = executor.transact(tx_version, tx_env);

        // The `​write_new_locations` mechanism optimizes validation by intelligently reducing
        // redundant verification tasks. Under standard validation logic, when a conflicted
        // transaction is re-executed, all subsequent transactions must undergo revalidation.
        // However, if the re-executed transaction hasn't written to any new storage locations (as
        // tracked by write_new_locations), subsequent transactions can skip this revalidation
        // process. This optimization significantly decreases the total number of required
        // validation tasks.
        let mut write_new_locations = false;
        let conflict;
        let mut next = None;
        match result {
            Ok(result_and_state) => {
                // only the miner involved in transaction should accumulate the rewards of finality
                // txs return true if the tx doesn't visit the miner account
                let read_accurate_origin = executor.db_mut().read_accurate_origin();

                let blocking_txs = executor.db_mut().take_estimate_txs();
                conflict = !read_accurate_origin || !blocking_txs.is_empty();
                let read_set = executor.db_mut().take_read_set();
                let write_set =
                    executor.db_mut().update_mv_memory(&result_and_state.state, conflict);

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

                if conflict {
                    self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
                    if !read_accurate_origin {
                        self.metrics.conflict_by_miner.fetch_add(1, Ordering::Relaxed);
                        // Add all previous transactions as dependencies if miner doesn't accumulate
                        // the rewards
                        self.tx_dependency.key_tx(txid, &self.scheduler_ctx.commit_idx);
                    } else {
                        self.metrics.conflict_by_estimate.fetch_add(1, Ordering::Relaxed);
                        self.tx_dependency.add(txid, self.generate_dependent_tx(txid, &read_set));
                    }
                } else {
                    // Grevm employs an optimized thread scheduling strategy that differs
                    // fundamentally from Block-STM's approach while intelligently preserving its
                    // advantages. Unlike Block-STM where conflicted transactions persistently
                    // occupy threads through busy-waiting retries, Grevm normally yields the thread
                    // and re-schedules via DAG - except in critical path scenarios where it
                    // demonstrates adaptive behavior. When detecting strictly linear dependencies
                    // (where the next transaction immediately depends on the current one), Grevm
                    // makes a crucial optimization: it maintains thread continuity by directly
                    // executing the dependent transaction within the same thread rather than
                    // yielding. This hybrid approach combines the general efficiency of DAG-based
                    // scheduling for parallelizable workloads with Block-STM's optimal performance
                    // for sequential dependency chains, effectively minimizing both thread
                    // contention and scheduling overhead. The system automatically applies the most
                    // appropriate execution strategy based on real-time dependency analysis,
                    // ensuring neither purely optimistic (Block-STM) nor purely DAG-driven
                    // approaches impose unnecessary performance penalties in their respective
                    // worst-case scenarios.
                    next = self.tx_dependency.remove(txid, true);
                }
                *last_result = Some(TransactionResult {
                    read_set,
                    write_set,
                    execute_result: Ok(result_and_state),
                });
            }
            Err(e) => {
                let recoverable = SkipReason::from_evm_error(&e).is_some();
                conflict = true;
                self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
                self.metrics.conflict_by_error.fetch_add(1, Ordering::Relaxed);
                let mut write_set = HashSet::new();

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_mut() {
                    write_set = std::mem::take(&mut last_result.write_set);
                    self.mark_estimate(txid, &write_set);
                }
                *last_result = Some(TransactionResult {
                    read_set: Default::default(),
                    write_set,
                    execute_result: Err(e),
                });
                if commit_idx == txid {
                    if recoverable {
                        self.abort(AbortReason::FallbackSequential);
                    } else {
                        self.abort(AbortReason::FatalEvmError(txid));
                    }
                }
                self.tx_dependency.key_tx(txid, &self.scheduler_ctx.commit_idx);
            }
        }

        tx_state.status =
            if conflict { TransactionStatus::Conflict } else { TransactionStatus::Executed };
        self.scheduler_ctx.executed(txid);

        if let Some(next) = next {
            self.scheduler_ctx.reset_validation_idx(txid);
            drop(tx_state);
            return self.execution_task(next);
        }
        if conflict {
            self.scheduler_ctx.reset_validation_idx(txid + 1);
        } else {
            if write_new_locations {
                self.scheduler_ctx.reset_validation_idx(txid);
            } else {
                tx_state.status = TransactionStatus::Validating;
                return Some(Task::Validation(TxVersion::new(txid, incarnation)));
            }
        }
        None
    }

    fn validate(&self, tx_version: TxVersion) -> Option<Task> {
        let TxVersion { txid, incarnation } = tx_version;
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
        self.metrics.validation_cnt.fetch_add(1, Ordering::Relaxed);
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

        let ts = self.scheduler_ctx.logical_timestamp();
        // check the read version of read set
        let mut conflict = false;
        let mut dependency: Option<TxId> = None;
        for (location, version) in result.read_set.iter() {
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
            self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
            self.metrics.conflict_by_version.fetch_add(1, Ordering::Relaxed);
            // mark write set as estimate
            self.mark_estimate(txid, &result.write_set);
        }

        // update transaction status
        tx_state.status = if conflict {
            self.scheduler_ctx.reset_validation_idx(txid + 1);
            TransactionStatus::Conflict
        } else {
            self.scheduler_ctx.unconfirmed(txid, ts);
            TransactionStatus::Unconfirmed
        };
        tx_state.dependency = dependency;

        if conflict {
            // update dependency
            let dep_tx = dependency.and_then(|dep| {
                if dep >= self.scheduler_ctx.finality_idx() { Some(dep) } else { None }
            });
            self.tx_dependency.add(txid, dep_tx);
        }
        None
    }

    fn mark_estimate(&self, txid: TxId, write_set: &HashSet<LocationAndType>) {
        for location in write_set {
            if let Some(mut written_transactions) = self.mv_memory.get_mut(location) &&
                let Some(entry) = written_transactions.get_mut(&txid)
            {
                entry.estimate = true;
            }
        }
    }

    fn generate_dependent_tx(
        &self,
        txid: TxId,
        read_set: &HashMap<LocationAndType, ReadVersion>,
    ) -> Option<TxId> {
        let mut max_dep_id = None;
        for location in read_set.keys() {
            if let Some(written_transactions) = self.mv_memory.get(location) &&
                let Some((&dep_id, _)) = written_transactions.range(..txid).next_back() &&
                max_dep_id.is_none_or(|current| dep_id > current) &&
                dep_id >= self.scheduler_ctx.finality_idx()
            {
                // To prevent dependency explosion, keep only the highest preceding transaction.
                max_dep_id = Some(dep_id);
                if dep_id == txid - 1 {
                    return max_dep_id;
                }
            }
        }
        max_dep_id
    }

    fn execution_task(&self, execute_id: TxId) -> Option<Task> {
        let mut tx = self.tx_states[execute_id].lock();
        if matches!(tx.status, TransactionStatus::Initial | TransactionStatus::Conflict) {
            tx.status = TransactionStatus::Executing;
            tx.incarnation += 1;
            Some(Task::Execution(TxVersion::new(execute_id, tx.incarnation)))
        } else {
            self.tx_dependency.remove(execute_id, false);
            self.metrics.useless_dependent_update.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    fn next(&self) -> Option<Task> {
        while !self.scheduler_ctx.finished() && !self.abort.load(Ordering::Acquire) {
            if !self.scheduler_ctx.should_schedule(self.tx_dependency.index()) {
                thread::yield_now();
            }

            if let Some(validation_idx) =
                self.scheduler_ctx.next_validation_idx(self.tx_dependency.index())
            {
                let mut tx = self.tx_states[validation_idx].lock();
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
