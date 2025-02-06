use crate::{
    async_commit::StateAsyncCommit, hint::ParallelExecutionHints, storage::CacheDB,
    tx_dependency::TxDependency, utils::LockFreeQueue, AbortReason, LocationAndType, MemoryEntry,
    ReadVersion, Task, TransactionResult, TransactionStatus, TxId, TxState, TxVersion,
    CONCURRENT_LEVEL,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use auto_impl::auto_impl;
use dashmap::DashMap;
use metrics::histogram;
use parking_lot::{Mutex, RwLock};
use revm::{db::states::ParallelState, Evm, EvmBuilder, StateBuilder};
use revm_primitives::{
    db::{DatabaseCommit, DatabaseRef},
    EVMError, Env, ExecutionResult, SpecId, TxEnv, TxKind,
};
use std::{
    cmp::max,
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, OnceLock,
    },
    thread,
    time::Instant,
};

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
    tx_dependency: Mutex<TxDependency>,

    mv_memory: MVMemory,

    finality_idx: AtomicUsize,
    validation_idx: AtomicUsize,
    execution_idx: AtomicUsize,
    num_commit: AtomicUsize,

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
            tx_dependency: Mutex::new(tx_dependency),
            mv_memory: MVMemory::new(),
            finality_idx: AtomicUsize::new(0),
            validation_idx: AtomicUsize::new(0),
            execution_idx: AtomicUsize::new(0),
            num_commit: AtomicUsize::new(0),
            abort: AtomicBool::new(false),
            abort_reason: OnceLock::new(),
            metrics: ExecuteMetricsCollector::default(),
        }
    }

    fn async_commit(
        &self,
        commiter: &Mutex<StateAsyncCommit<DB>>,
        task_queue: &LockFreeQueue<Task>,
    ) {
        let mut start = Instant::now();
        let mut num_commit = 0;
        let mut commiter = commiter.lock();
        let async_commit_state =
            std::env::var("ASYNC_COMMIT_STATE").map_or(true, |s| s.parse().unwrap());
        while !self.abort.load(Ordering::Relaxed) && num_commit < self.block_size {
            let finality_idx = self.finality_idx.load(Ordering::Relaxed);
            if num_commit == finality_idx {
                thread::yield_now();
            } else {
                while num_commit < finality_idx {
                    if async_commit_state {
                        let tx = self.tx_results[num_commit].lock();
                        let result = tx.as_ref().unwrap().execute_result.clone();
                        let Ok(result) = result else { panic!("Commit error tx: {}", num_commit) };
                        commiter.commit(result);
                    }
                    num_commit += 1;
                    self.num_commit.fetch_add(1, Ordering::Relaxed);
                }
            }
            if (Instant::now() - start).as_millis() > 8_000 {
                start = Instant::now();
                println!(
                    "stuck..., finality_idx: {}, validation_idx: {}, execution_idx: {}",
                    self.finality_idx.load(Ordering::Acquire),
                    self.validation_idx.load(Ordering::Acquire),
                    self.execution_idx.load(Ordering::Acquire)
                );
                let status: Vec<(TxId, TransactionStatus)> =
                    self.tx_states.iter().map(|s| s.lock().status.clone()).enumerate().collect();
                println!("transaction status: {:?}", status);
                self.tx_dependency.lock().print();
                println!("task queue size: {}", task_queue.len());
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
    ) -> Result<(), EVMError<DB::Error>> {
        self.metrics.total_tx_cnt.store(self.block_size, Ordering::Relaxed);
        let concurrency_level = concurrency_level.unwrap_or(*CONCURRENT_LEVEL);
        let task_queue = LockFreeQueue::new(concurrency_level * 4);
        let commiter = Mutex::new(StateAsyncCommit::new(self.env.block.coinbase, &self.state));
        commiter.lock().init().map_err(|e| EVMError::Database(e))?;
        thread::scope(|scope| {
            scope.spawn(|| {
                self.async_commit(&commiter, &task_queue);
            });
            scope.spawn(|| {
                self.assign_tasks(&task_queue);
            });
            for _ in 0..concurrency_level {
                scope.spawn(|| {
                    let mut cache_db = CacheDB::new(
                        self.env.block.coinbase,
                        &self.state,
                        &self.mv_memory,
                        &self.num_commit,
                    );
                    let mut evm = EvmBuilder::default()
                        .with_db(&mut cache_db)
                        .with_spec_id(self.spec_id.clone())
                        .with_env(Box::new(self.env.clone()))
                        .build();
                    while let Some(task) = self.next(&task_queue) {
                        match task {
                            Task::Execution(tx_version) => {
                                self.execute(&mut evm, tx_version);
                            }
                            Task::Validation(tx_version) => {
                                self.validate(tx_version);
                            }
                        }
                    }
                });
            }
        });
        self.results.lock().extend(commiter.lock().take_result());
        self.post_execute()
    }

    fn post_execute(&self) -> Result<(), EVMError<DB::Error>> {
        if self.abort.load(Ordering::Relaxed) {
            if let Some(abort_reason) = self.abort_reason.get() {
                match abort_reason {
                    AbortReason::EvmError => {
                        let result =
                            self.tx_results[self.finality_idx.load(Ordering::Relaxed)].lock();
                        if let Some(result) = result.as_ref() {
                            if let Err(e) = &result.execute_result {
                                return Err(e.clone());
                            }
                        }
                        panic!("Wrong abort transaction")
                    }
                    AbortReason::SelfDestructed => {
                        return self.fallback_sequential();
                    }
                }
            }
        }
        self.metrics.report();
        Ok(())
    }

    pub fn fallback_sequential(&self) -> Result<(), EVMError<DB::Error>> {
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
                let result_and_state = evm.transact()?;
                evm.db_mut().commit(result_and_state.state);
                sequential_results.push(result_and_state.result);
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
    ) {
        self.metrics.execution_cnt.fetch_add(1, Ordering::Relaxed);
        let num_commit = self.num_commit.load(Ordering::Relaxed);
        let TxVersion { txid, incarnation } = tx_version;
        evm.db_mut().reset_state(TxVersion::new(txid, incarnation));
        let mut tx_env = self.txs[txid].clone();
        tx_env.nonce = None;
        *evm.tx_mut() = tx_env;
        let result = evm.transact_lazy_reward();

        match result {
            Ok(result_and_state) => {
                // only the miner involved in transaction should accumulate the rewards of finality
                // txs return true if the tx doesn't visit the miner account
                let read_accurate_origin = evm.db().read_accurate_origin();
                let blocking_txs = evm.db_mut().take_estimate_txs();
                let conflict = !read_accurate_origin || !blocking_txs.is_empty();
                let read_set = evm.db_mut().take_read_set();
                let write_set = evm.db().update_mv_memory(&result_and_state.state, conflict);

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_ref() {
                    for location in &last_result.write_set {
                        if !write_set.contains(location) {
                            if let Some(mut written_transactions) = self.mv_memory.get_mut(location)
                            {
                                written_transactions.remove(&txid);
                            }
                        }
                    }
                }

                // update transaction status
                {
                    let mut tx_state = self.tx_states[txid].lock();
                    if tx_state.incarnation != incarnation {
                        panic!("Inconsistent incarnation when execution");
                    }
                    tx_state.status = if conflict {
                        TransactionStatus::Conflict
                    } else {
                        TransactionStatus::Executed
                    };
                }

                if conflict {
                    self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
                    if !read_accurate_origin {
                        self.metrics.conflict_by_miner.fetch_add(1, Ordering::Relaxed);
                        // Add all previous transactions as dependencies if miner doesn't accumulate
                        // the rewards
                        self.tx_dependency.lock().add(txid, Some(txid - 1));
                    } else {
                        self.metrics.conflict_by_estimate.fetch_add(1, Ordering::Relaxed);
                        self.tx_dependency
                            .lock()
                            .add(txid, self.generate_dependent_tx(txid, &read_set));
                    }
                } else if write_set.len() < 8 {
                    // When write set is large, the transaction can be more complex
                    // Update dependency when finality
                    self.tx_dependency.lock().remove(txid);
                }
                *last_result = Some(TransactionResult {
                    read_set,
                    write_set,
                    execute_result: Ok(result_and_state),
                });
            }
            Err(e) => {
                self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
                self.metrics.conflict_by_error.fetch_add(1, Ordering::Relaxed);
                let mut write_set = HashSet::new();

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_mut() {
                    write_set = std::mem::take(&mut last_result.write_set);
                    self.mark_estimate(txid, &write_set);
                }

                // update transaction status
                {
                    let mut tx_state = self.tx_states[txid].lock();
                    if tx_state.incarnation != incarnation {
                        panic!("Inconsistent incarnation when execution");
                    }
                    tx_state.status = TransactionStatus::Conflict;
                }
                self.tx_dependency.lock().add(txid, if txid > 0 { Some(txid - 1) } else { None });
                *last_result = Some(TransactionResult {
                    read_set: Default::default(),
                    write_set,
                    execute_result: Err(e),
                });
                if num_commit == txid {
                    self.abort(AbortReason::EvmError);
                }
            }
        }
    }

    fn validate(&self, tx_version: TxVersion) {
        self.metrics.validation_cnt.fetch_add(1, Ordering::Relaxed);
        let TxVersion { txid, incarnation } = tx_version;
        // check the read version of read set
        let mut conflict = false;
        let tx_result = self.tx_results[txid].lock();
        let Some(result) = tx_result.as_ref() else {
            panic!("No result when validating");
        };
        if let Err(_) = &result.execute_result {
            panic!("Error transaction should take as conflict before validating");
        }

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
        {
            let mut tx_state = self.tx_states[txid].lock();
            if tx_state.incarnation != incarnation {
                panic!("Inconsistent incarnation when validating");
            }
            tx_state.status =
                if conflict { TransactionStatus::Conflict } else { TransactionStatus::Unconfirmed };
            tx_state.has_dependency = dependency.is_some();
        }

        if conflict {
            // update dependency
            let dep_tx = dependency.and_then(|dep| {
                if dep >= self.finality_idx.load(Ordering::Relaxed) {
                    Some(dep)
                } else {
                    None
                }
            });
            self.tx_dependency.lock().add(txid, dep_tx);
        }
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
                        dep_id >= self.finality_idx.load(Ordering::Relaxed)
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

    fn assign_tasks(&self, task_queue: &LockFreeQueue<Task>) {
        while self.finality_idx.load(Ordering::Relaxed) < self.block_size &&
            !self.abort.load(Ordering::Relaxed)
        {
            let mut finality_idx = self.finality_idx.load(Ordering::Acquire);
            let mut validation_idx = self.validation_idx.load(Ordering::Acquire);
            let mut execution_idx = self.execution_idx.load(Ordering::Acquire);
            // Confirm the finality and conflict status
            let origin_finality_idx = finality_idx;
            if finality_idx < validation_idx {
                while finality_idx < validation_idx {
                    let mut tx = self.tx_states[finality_idx].lock();
                    match tx.status {
                        TransactionStatus::Unconfirmed => {
                            tx.status = TransactionStatus::Finality;
                            finality_idx += 1;
                            if !tx.has_dependency {
                                self.metrics.no_dependency_txs.fetch_add(1, Ordering::Relaxed);
                            } else {
                                if tx.incarnation == 1 {
                                    self.metrics
                                        .one_attempt_with_dependency
                                        .fetch_add(1, Ordering::Relaxed);
                                } else if tx.incarnation > 2 {
                                    self.metrics
                                        .more_attempts_with_dependency
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        TransactionStatus::Conflict | TransactionStatus::Executed => {
                            self.metrics.reset_validation_idx_cnt.fetch_add(1, Ordering::Relaxed);
                            // Subsequent transactions after conflict need to be revalidated
                            validation_idx = finality_idx;
                            break;
                        }
                        _ => {
                            break;
                        }
                    }
                }
                if finality_idx > origin_finality_idx {
                    self.finality_idx.store(finality_idx, Ordering::Release);
                    let mut tx_dependency = self.tx_dependency.lock();
                    for txid in origin_finality_idx..finality_idx {
                        tx_dependency.commit(txid);
                    }
                }
            }

            // Prior to submit validation task
            let mut num_tasks = 0;
            let num_validation = task_queue.capacity() - task_queue.len();
            while validation_idx < execution_idx && num_tasks < num_validation {
                let mut tx = self.tx_states[validation_idx].lock();
                if matches!(tx.status, TransactionStatus::Executed | TransactionStatus::Unconfirmed)
                {
                    tx.status = TransactionStatus::Validating;
                    task_queue
                        .push(Task::Validation(TxVersion::new(validation_idx, tx.incarnation)));
                    num_tasks += 1;
                    validation_idx += 1;
                } else {
                    break;
                }
            }
            self.validation_idx.store(validation_idx, Ordering::Release);

            // Submit execution task
            let num_execute = task_queue.capacity() - task_queue.len();
            if num_execute > 0 {
                let mut tx_dependency = self.tx_dependency.lock();
                for execute_id in tx_dependency.next(num_execute) {
                    if execute_id < finality_idx {
                        self.metrics.useless_dependent_update.fetch_add(1, Ordering::Relaxed);
                        tx_dependency.commit(execute_id);
                        continue;
                    }
                    let mut tx = self.tx_states[execute_id].lock();
                    if !matches!(
                        tx.status,
                        TransactionStatus::Initial | TransactionStatus::Conflict
                    ) {
                        if tx.status != TransactionStatus::Executing {
                            self.metrics.useless_dependent_update.fetch_add(1, Ordering::Relaxed);
                            tx_dependency.remove(execute_id);
                        }
                        continue;
                    }
                    execution_idx = max(execution_idx, execute_id + 1);
                    tx.status = TransactionStatus::Executing;
                    tx.incarnation += 1;
                    task_queue.push(Task::Execution(TxVersion::new(execute_id, tx.incarnation)));
                    num_tasks += 1;
                }
            }
            self.execution_idx.store(execution_idx, Ordering::Release);

            if num_tasks == 0 {
                thread::yield_now();
            }
        }
    }

    pub fn next(&self, task_queue: &LockFreeQueue<Task>) -> Option<Task> {
        while self.finality_idx.load(Ordering::Relaxed) < self.block_size &&
            !self.abort.load(Ordering::Relaxed)
        {
            if let Some(task) = task_queue.multi_pop() {
                return Some(task);
            } else {
                thread::yield_now();
            }
        }
        None
    }
}
