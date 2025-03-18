use crate::{
    async_commit::StateAsyncCommit,
    hint::ParallelExecutionHints,
    storage::CacheDB,
    tx_dependency::TxDependency,
    utils::{ContinuousDetectSet, LockFreeQueue},
    AbortReason, GrevmError, LocationAndType, MemoryEntry, ParallelState, ReadVersion, Task,
    TransactionResult, TransactionStatus, TxId, TxState, TxVersion, CONCURRENT_LEVEL,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use dashmap::DashMap;
use metrics::histogram;
use parking_lot::{Mutex, RwLock};
use revm::{Evm, EvmBuilder};
use revm_primitives::{
    db::{DatabaseCommit, DatabaseRef},
    EVMError, Env, ExecutionResult, SpecId, TxEnv,
};
use std::{
    cmp::max,
    collections::BTreeMap,
    fmt::Display,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    thread,
    time::Instant,
};
use tracing::*;

pub type MVMemory = DashMap<LocationAndType, BTreeMap<TxId, MemoryEntry>>;

struct ExecuteMetrics {
    /// Total number of transactions.
    total_tx_cnt: metrics::Histogram,
    conflict_cnt: metrics::Histogram,
    validation_cnt: metrics::Histogram,
    execution_cnt: metrics::Histogram,
    reset_validation_idx_cnt: metrics::Histogram,
    useless_dependent_update: metrics::Histogram,

    conflict_by_miner: metrics::Histogram,
    conflict_by_error: metrics::Histogram,
    conflict_by_estimate: metrics::Histogram,
    conflict_by_version: metrics::Histogram,
    one_attempt_with_dependency: metrics::Histogram,
    more_attempts_with_dependency: metrics::Histogram,
    no_dependency_txs: metrics::Histogram,
    conflict_txs: metrics::Histogram,
}

impl Default for ExecuteMetrics {
    fn default() -> Self {
        Self {
            total_tx_cnt: histogram!("grevm.total_tx_cnt"),
            conflict_cnt: histogram!("grevm.conflict_cnt"),
            validation_cnt: histogram!("grevm.validation_cnt"),
            execution_cnt: histogram!("grevm.execution_cnt"),
            reset_validation_idx_cnt: histogram!("grevm.reset_validation_idx_cnt"),
            useless_dependent_update: histogram!("grevm.useless_dependent_update"),
            conflict_by_miner: histogram!("grevm.conflict_by_miner"),
            conflict_by_error: histogram!("grevm.conflict_by_error"),
            conflict_by_estimate: histogram!("grevm.conflict_by_estimate"),
            conflict_by_version: histogram!("grevm.conflict_by_version"),
            one_attempt_with_dependency: histogram!("grevm.one_attempt_with_dependency"),
            more_attempts_with_dependency: histogram!("grevm.more_attempts_with_dependency"),
            no_dependency_txs: histogram!("grevm.no_dependency_txs"),
            conflict_txs: histogram!("grevm.conflict_txs"),
        }
    }
}

#[derive(Default)]
struct ExecuteMetricsCollector {
    total_tx_cnt: AtomicUsize,
    conflict_cnt: AtomicUsize,
    validation_cnt: AtomicUsize,
    execution_cnt: AtomicUsize,
    reset_validation_idx_cnt: AtomicUsize,
    useless_dependent_update: AtomicUsize,

    conflict_by_miner: AtomicUsize,
    conflict_by_error: AtomicUsize,
    conflict_by_estimate: AtomicUsize,
    conflict_by_version: AtomicUsize,
    one_attempt_with_dependency: AtomicUsize,
    more_attempts_with_dependency: AtomicUsize,
    no_dependency_txs: AtomicUsize,
    conflict_txs: AtomicUsize,
}

impl ExecuteMetricsCollector {
    fn report(&self) {
        let execute_metrics = ExecuteMetrics::default();
        execute_metrics.total_tx_cnt.record(self.total_tx_cnt.load(Ordering::Relaxed) as f64);
        execute_metrics.conflict_cnt.record(self.conflict_cnt.load(Ordering::Relaxed) as f64);
        execute_metrics.validation_cnt.record(self.validation_cnt.load(Ordering::Relaxed) as f64);
        execute_metrics.execution_cnt.record(self.execution_cnt.load(Ordering::Relaxed) as f64);
        execute_metrics
            .reset_validation_idx_cnt
            .record(self.reset_validation_idx_cnt.load(Ordering::Relaxed) as f64);
        execute_metrics
            .useless_dependent_update
            .record(self.useless_dependent_update.load(Ordering::Relaxed) as f64);
        execute_metrics
            .conflict_by_miner
            .record(self.conflict_by_miner.load(Ordering::Relaxed) as f64);
        execute_metrics
            .conflict_by_error
            .record(self.conflict_by_error.load(Ordering::Relaxed) as f64);
        execute_metrics
            .conflict_by_estimate
            .record(self.conflict_by_estimate.load(Ordering::Relaxed) as f64);
        execute_metrics
            .conflict_by_version
            .record(self.conflict_by_version.load(Ordering::Relaxed) as f64);
        execute_metrics
            .one_attempt_with_dependency
            .record(self.one_attempt_with_dependency.load(Ordering::Relaxed) as f64);
        execute_metrics
            .more_attempts_with_dependency
            .record(self.more_attempts_with_dependency.load(Ordering::Relaxed) as f64);
        execute_metrics
            .no_dependency_txs
            .record(self.no_dependency_txs.load(Ordering::Relaxed) as f64);
        execute_metrics.conflict_txs.record(self.conflict_txs.load(Ordering::Relaxed) as f64);
    }
}

struct SchedulerContext {
    num_txs: usize,
    validation_idx: AtomicUsize,
    finality_idx: AtomicUsize,
    commit_idx: AtomicUsize,
    executed_set: ContinuousDetectSet,
    reset_validation_idx_cnt: AtomicUsize,

    logical_ts: AtomicUsize,
    lower_ts: Vec<AtomicUsize>,
    unconfirmed_ts: Vec<AtomicUsize>,
}

impl SchedulerContext {
    fn new(num_txs: usize) -> Self {
        Self {
            num_txs,
            validation_idx: AtomicUsize::new(0),
            finality_idx: AtomicUsize::new(0),
            commit_idx: AtomicUsize::new(0),
            executed_set: ContinuousDetectSet::new(num_txs),
            reset_validation_idx_cnt: AtomicUsize::new(0),
            logical_ts: AtomicUsize::new(1),
            lower_ts: (0..num_txs).map(|_| AtomicUsize::new(0)).collect(),
            unconfirmed_ts: (0..num_txs).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    fn reset_validation_idx(&self, index: usize) {
        if index < self.num_txs {
            let ts = self.logical_ts.fetch_add(1, Ordering::Relaxed);
            self.lower_ts[index].fetch_max(ts, Ordering::Release);
            let prev = self.validation_idx.fetch_min(index, Ordering::Relaxed);
            if prev > index {
                self.reset_validation_idx_cnt.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn logical_timestamp(&self) -> usize {
        self.logical_ts.fetch_add(1, Ordering::Relaxed)
    }

    fn executed(&self, index: usize) {
        self.executed_set.add(index);
    }

    fn unconfirmed(&self, index: usize, ts: usize) {
        self.unconfirmed_ts[index].fetch_max(ts, Ordering::Release);
    }

    fn finished(&self) -> bool {
        self.finality_idx.load(Ordering::Relaxed) >= self.num_txs
    }

    fn finality_idx(&self) -> usize {
        self.finality_idx.load(Ordering::Relaxed)
    }

    fn validation_idx(&self) -> usize {
        self.validation_idx.load(Ordering::Relaxed)
    }

    fn should_shedule(&self, executing_idx: usize) -> bool {
        let validation_idx = self.validation_idx.load(Ordering::Relaxed);
        let should_validation =
            validation_idx < executing_idx && validation_idx < self.executed_set.continuous_idx();
        let should_execution = executing_idx < self.num_txs;
        should_validation || should_execution
    }

    fn next_validation_idx(&self, executing_idx: usize) -> Option<usize> {
        let validation_idx = self.validation_idx.load(Ordering::Relaxed);
        if validation_idx < executing_idx && validation_idx < self.executed_set.continuous_idx() {
            let validation_idx = self.validation_idx.fetch_add(1, Ordering::Relaxed);
            if validation_idx < self.num_txs {
                return Some(validation_idx);
            }
        }
        None
    }
}

pub struct Scheduler<DB>
where
    DB: DatabaseRef,
{
    spec_id: SpecId,
    env: Env,
    block_size: usize,
    txs: Arc<Vec<TxEnv>>,
    state: ParallelState<DB>,
    results: Mutex<Vec<ExecutionResult>>,
    tx_states: Vec<Mutex<TxState>>,
    tx_results: Vec<Mutex<Option<TransactionResult<DB::Error>>>>,
    tx_dependency: TxDependency,

    mv_memory: MVMemory,
    scheduler_ctx: SchedulerContext,

    abort: AtomicBool,
    abort_reason: OnceLock<AbortReason>,
    metrics: ExecuteMetricsCollector,
}

impl<DB> Scheduler<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync,
{
    pub fn new(
        spec_id: SpecId,
        env: Env,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
    ) -> Self {
        let num_txs = txs.len();
        let tx_dependency = if with_hints {
            ParallelExecutionHints::new(txs.clone()).parse_hints()
        } else {
            TxDependency::new(num_txs)
        };
        Self {
            spec_id,
            env,
            block_size: num_txs,
            txs,
            state,
            results: Mutex::new(vec![]),
            tx_states: (0..num_txs).map(|_| Mutex::new(TxState::default())).collect(),
            tx_results: (0..num_txs).map(|_| Mutex::new(None)).collect(),
            tx_dependency,
            mv_memory: MVMemory::new(),
            scheduler_ctx: SchedulerContext::new(num_txs),
            abort: AtomicBool::new(false),
            abort_reason: OnceLock::new(),
            metrics: ExecuteMetricsCollector::default(),
        }
    }

    fn async_commit(&self, commiter: &Mutex<StateAsyncCommit<DB>>) {
        let mut start = Instant::now();
        let mut commit_idx = 0;
        let mut lower_ts = 0;
        let mut commiter = commiter.lock();
        let async_commit_state =
            std::env::var("ASYNC_COMMIT_STATE").map_or(true, |s| s.parse().unwrap());
        let dependency_distance = histogram!("grevm.dependency_distance");
        while !self.abort.load(Ordering::Relaxed) && commit_idx < self.block_size {
            while commit_idx < self.block_size && commit_idx < self.scheduler_ctx.validation_idx() {
                if self.tx_states[commit_idx].lock().status != TransactionStatus::Unconfirmed {
                    break;
                }
                lower_ts =
                    max(lower_ts, self.scheduler_ctx.lower_ts[commit_idx].load(Ordering::Acquire));
                if self.scheduler_ctx.unconfirmed_ts[commit_idx].load(Ordering::Acquire) <= lower_ts
                {
                    break;
                }
                let mut tx_state = self.tx_states[commit_idx].lock();
                tx_state.status = TransactionStatus::Finality;
                self.scheduler_ctx.finality_idx.fetch_add(1, Ordering::Relaxed);
                if async_commit_state {
                    let tx_result = self.tx_results[commit_idx].lock();
                    let result = tx_result.as_ref().unwrap().execute_result.clone();
                    let Ok(result) = result else { panic!("Commit error tx: {}", commit_idx) };
                    commiter.commit(commit_idx, &self.txs[commit_idx], result);
                    if commiter.commit_result().is_err() {
                        self.abort(AbortReason::EvmError);
                        return;
                    }
                }

                if tx_state.incarnation > 1 {
                    self.metrics.conflict_txs.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(dep_id) = tx_state.dependency {
                    dependency_distance.record((commit_idx - dep_id) as f64);
                    if tx_state.incarnation == 1 {
                        self.metrics.one_attempt_with_dependency.fetch_add(1, Ordering::Relaxed);
                    } else if tx_state.incarnation > 2 {
                        self.metrics.more_attempts_with_dependency.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    self.metrics.no_dependency_txs.fetch_add(1, Ordering::Relaxed);
                }
                commit_idx += 1;
                self.scheduler_ctx.commit_idx.fetch_add(1, Ordering::Relaxed);
            }
            thread::yield_now();

            if (Instant::now() - start).as_millis() > 8_000 {
                start = Instant::now();
                println!(
                    "stuck..., block_number: {}, finality_idx: {}, validation_idx: {}, execution_idx: {}",
                    self.env.block.number,
                    self.scheduler_ctx.finality_idx(),
                    self.scheduler_ctx.validation_idx(),
                    self.scheduler_ctx.executed_set.continuous_idx(),
                );
                let status: Vec<(TxId, TransactionStatus)> =
                    self.tx_states.iter().map(|s| s.lock().status.clone()).enumerate().collect();
                println!("transaction status: {:?}", status);
                self.tx_dependency.print();
            }
        }
    }

    fn state_mut(&self) -> &mut ParallelState<DB> {
        #[allow(invalid_reference_casting)]
        unsafe {
            &mut *(&self.state as *const ParallelState<DB> as *mut ParallelState<DB>)
        }
    }

    pub fn take_result_and_state(mut self) -> (Vec<ExecutionResult>, ParallelState<DB>) {
        (self.results.into_inner(), self.state)
    }

    pub fn parallel_execute(
        &self,
        concurrency_level: Option<usize>,
    ) -> Result<(), GrevmError<DB::Error>> {
        self.metrics.total_tx_cnt.store(self.block_size, Ordering::Relaxed);
        let concurrency_level = concurrency_level.unwrap_or(
            std::env::var("GREVM_CONCURRENT_LEVEL")
                .map_or(*CONCURRENT_LEVEL, |s| s.parse().unwrap()),
        );
        let commiter = Mutex::new(StateAsyncCommit::new(self.env.block.coinbase, &self.state));
        commiter.lock().init().map_err(|e| GrevmError { txid: 0, error: EVMError::Database(e) })?;
        thread::scope(|scope| {
            scope.spawn(|| {
                self.async_commit(&commiter);
            });
            for _ in 0..concurrency_level {
                scope.spawn(|| {
                    let mut cache_db = CacheDB::new(
                        self.env.block.coinbase,
                        &self.state,
                        &self.mv_memory,
                        &self.scheduler_ctx.commit_idx,
                    );
                    let mut evm = EvmBuilder::default()
                        .with_db(&mut cache_db)
                        .with_spec_id(self.spec_id.clone())
                        .with_env(Box::new(self.env.clone()))
                        .build();
                    let mut task = self.next();
                    while task.is_some() {
                        task = match task.unwrap() {
                            Task::Execution(tx_version) => self.execute(&mut evm, tx_version),
                            Task::Validation(tx_version) => self.validate(tx_version),
                        };
                        if task.is_none() && !self.abort.load(Ordering::Relaxed) {
                            task = self.next();
                        }
                    }
                });
            }
        });
        {
            let mut commiter = commiter.lock();
            if let Err(e) = commiter.commit_result() {
                return Err(e.clone());
            }
            self.results.lock().extend(commiter.take_result());
        }
        self.post_execute()?;
        self.metrics.reset_validation_idx_cnt.store(
            self.scheduler_ctx.reset_validation_idx_cnt.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.metrics.report();
        Ok(())
    }

    fn post_execute(&self) -> Result<(), GrevmError<DB::Error>> {
        if self.abort.load(Ordering::Relaxed) {
            if let Some(abort_reason) = self.abort_reason.get() {
                match abort_reason {
                    AbortReason::EvmError => {
                        let txid = self.scheduler_ctx.finality_idx();
                        let result = self.tx_results[txid].lock();
                        if let Some(result) = result.as_ref() {
                            if let Err(e) = &result.execute_result {
                                return Err(GrevmError { txid, error: e.clone() });
                            }
                        }
                        panic!("Wrong abort transaction")
                    }
                    AbortReason::SelfDestructed | AbortReason::FallbackSequential => {
                        return self.fallback_sequential();
                    }
                }
            }
        }
        Ok(())
    }

    pub fn fallback_sequential(&self) -> Result<(), GrevmError<DB::Error>> {
        let mut results = self.results.lock();
        let num_commit = results.len();
        if num_commit == self.block_size {
            return Ok(());
        }

        let mut sequential_results = Vec::with_capacity(self.block_size - num_commit);
        let state_mut = self.state_mut();
        {
            let mut evm = EvmBuilder::default()
                .with_db(state_mut)
                .with_spec_id(self.spec_id)
                .with_env(Box::new(self.env.clone()))
                .build();
            for txid in num_commit..self.block_size {
                *evm.tx_mut() = self.txs[txid].clone();
                let result_and_state =
                    evm.transact().map_err(|e| GrevmError { txid, error: e.clone() })?;
                evm.db_mut().commit(result_and_state.state);
                sequential_results.push(result_and_state.result);
                self.metrics.execution_cnt.fetch_add(1, Ordering::Relaxed);
            }
        }
        results.extend(sequential_results);
        return Ok(());
    }

    fn abort(&self, abort_reason: AbortReason) {
        self.abort_reason.get_or_init(|| abort_reason);
        self.abort.store(true, Ordering::Relaxed);
    }

    fn execute(
        &self,
        evm: &mut Evm<'_, (), &mut CacheDB<ParallelState<DB>>>,
        tx_version: TxVersion,
    ) -> Option<Task> {
        let TxVersion { txid, incarnation } = tx_version;
        let mut tx_state = self.tx_states[txid].lock();
        if tx_state.status != TransactionStatus::Executing {
            return None;
        }
        if tx_state.incarnation != incarnation {
            panic!("Inconsistent incarnation when execution");
        }
        self.metrics.execution_cnt.fetch_add(1, Ordering::Relaxed);

        evm.db_mut().reset_state(TxVersion::new(txid, incarnation));
        let mut tx_env = self.txs[txid].clone();
        tx_env.nonce = None;
        *evm.tx_mut() = tx_env;
        let commit_idx = self.scheduler_ctx.commit_idx.load(Ordering::Relaxed);
        let result = evm.transact_lazy_reward();

        let mut write_new_locations = false;
        let mut conflict = false;
        let mut next = None;
        match result {
            Ok(result_and_state) => {
                // only the miner involved in transaction should accumulate the rewards of finality
                // txs return true if the tx doesn't visit the miner account
                let read_accurate_origin = evm.db().read_accurate_origin();
                let blocking_txs = evm.db_mut().take_estimate_txs();
                conflict = !read_accurate_origin || !blocking_txs.is_empty();
                let read_set = evm.db_mut().take_read_set();
                let write_set = evm.db().update_mv_memory(&result_and_state.state, conflict);

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_ref() {
                    for location in write_set.iter() {
                        if !last_result.write_set.contains(location) {
                            write_new_locations = true;
                            break;
                        }
                    }
                    for location in &last_result.write_set {
                        if !write_set.contains(location) {
                            if let Some(mut written_transactions) = self.mv_memory.get_mut(location)
                            {
                                written_transactions.remove(&txid);
                            }
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
                        self.tx_dependency.add(txid, Some(txid - 1));
                    } else {
                        self.metrics.conflict_by_estimate.fetch_add(1, Ordering::Relaxed);
                        self.tx_dependency.add(txid, self.generate_dependent_tx(txid, &read_set));
                    }
                } else {
                    next = self.tx_dependency.remove(txid, true);
                }
                *last_result = Some(TransactionResult {
                    read_set,
                    write_set,
                    execute_result: Ok(result_and_state),
                });
            }
            Err(e) => {
                conflict = true;
                self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
                self.metrics.conflict_by_error.fetch_add(1, Ordering::Relaxed);
                let mut write_set = HashSet::new();

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_mut() {
                    write_set = std::mem::take(&mut last_result.write_set);
                    self.mark_estimate(txid, &write_set);
                }

                self.tx_dependency.add(txid, if txid > 0 { Some(txid - 1) } else { None });
                *last_result = Some(TransactionResult {
                    read_set: Default::default(),
                    write_set,
                    execute_result: Err(e),
                });
                if commit_idx == txid {
                    self.abort(AbortReason::EvmError);
                }
            }
        }

        tx_state.status =
            if conflict { TransactionStatus::Conflict } else { TransactionStatus::Executed };
        self.scheduler_ctx.executed(txid);

        if let Some(next) = next {
            if txid < self.scheduler_ctx.validation_idx() {
                self.scheduler_ctx.reset_validation_idx(txid);
            }
            drop(tx_state);
            return self.execution_task(next);
        }
        if txid < self.scheduler_ctx.validation_idx() {
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
            panic!("Inconsistent incarnation when validating");
        }
        self.metrics.validation_cnt.fetch_add(1, Ordering::Relaxed);
        let Some(result) = tx_result.as_ref() else {
            panic!("No result when validating");
        };
        if let Err(_) = &result.execute_result {
            panic!("Error transaction should take as conflict before validating");
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
            if txid < self.scheduler_ctx.validation_idx() {
                self.scheduler_ctx.reset_validation_idx(txid + 1);
            }
            TransactionStatus::Conflict
        } else {
            self.scheduler_ctx.unconfirmed(txid, ts);
            TransactionStatus::Unconfirmed
        };
        tx_state.dependency = dependency;

        if conflict {
            // update dependency
            let dep_tx = dependency.and_then(|dep| {
                if dep >= self.scheduler_ctx.finality_idx() {
                    Some(dep)
                } else {
                    None
                }
            });
            self.tx_dependency.add(txid, dep_tx);
        }
        None
    }

    fn mark_estimate(&self, txid: TxId, write_set: &HashSet<LocationAndType>) {
        for location in write_set {
            if let Some(mut written_transactions) = self.mv_memory.get_mut(location) {
                if let Some(entry) = written_transactions.get_mut(&txid) {
                    entry.estimate = true;
                }
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
            if let Some(written_transactions) = self.mv_memory.get(location) {
                // To prevent dependency explosion, only add the tx with the highest TxId in
                // written_transactions
                if let Some((&dep_id, _)) = written_transactions.range(..txid).next_back() {
                    if (max_dep_id.is_none() || dep_id > max_dep_id.unwrap()) &&
                        dep_id >= self.scheduler_ctx.finality_idx()
                    {
                        max_dep_id = Some(dep_id);
                        if dep_id == txid - 1 {
                            return max_dep_id;
                        }
                    }
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

    pub fn next(&self) -> Option<Task> {
        while !self.scheduler_ctx.finished() && !self.abort.load(Ordering::Relaxed) {
            if !self.scheduler_ctx.should_shedule(self.tx_dependency.index()) {
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

            if let Some(execute_id) = self.tx_dependency.next() {
                if let Some(task) = self.execution_task(execute_id) {
                    return Some(task);
                }
            }
        }
        None
    }
}
