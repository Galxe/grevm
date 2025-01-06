use crate::{
    async_commit::AsyncCommit,
    hint::ParallelExecutionHints,
    storage::{CacheDB, CachedStorageData},
    tx_dependency::TxDependency,
    utils::LockFreeQueue,
    LocationAndType, MemoryEntry, ReadVersion, Task, TransactionResult, TransactionStatus, TxId,
    TxState, TxVersion, CONCURRENT_LEVEL,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use auto_impl::auto_impl;
use dashmap::DashMap;
use metrics::histogram;
use parking_lot::{Mutex, RwLock};
use revm::{Evm, EvmBuilder};
use revm_primitives::{db::DatabaseRef, EVMError, Env, SpecId, TxEnv, TxKind};
use std::{
    cmp::max,
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};

pub type MVMemory = DashMap<LocationAndType, BTreeMap<TxId, MemoryEntry>>;

#[auto_impl(&)]
pub trait RewardsAccumulator {
    fn accumulate(&self, from: Option<TxId>, to: TxId) -> Option<u128>;
}

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

pub struct Scheduler<DB, C>
where
    DB: DatabaseRef,
    C: AsyncCommit,
{
    spec_id: SpecId,
    env: Env,
    block_size: usize,
    txs: Arc<Vec<TxEnv>>,
    db: DB,
    commiter: Mutex<C>,
    cache: CachedStorageData,
    tx_states: Vec<RwLock<TxState>>,
    tx_results: Vec<Mutex<Option<TransactionResult<DB::Error>>>>,
    tx_dependency: Mutex<TxDependency>,

    mv_memory: MVMemory,

    finality_idx: AtomicUsize,
    validation_idx: AtomicUsize,
    execution_idx: AtomicUsize,

    abort: AtomicBool,
    metrics: ExecuteMetricsCollector,
}

impl<DB, C> RewardsAccumulator for Scheduler<DB, C>
where
    DB: DatabaseRef,
    C: AsyncCommit,
{
    fn accumulate(&self, from: Option<TxId>, to: TxId) -> Option<u128> {
        // register miner accumulator
        if to > 0 {
            if self.tx_states[to - 1].read().status != TransactionStatus::Finality {
                return None;
            }
        }
        let mut rewards = 0;
        for prev in from.unwrap_or(0)..to {
            let tx_result = self.tx_results[prev].lock();
            let Some(result) = tx_result.as_ref() else {
                return None;
            };
            let Ok(result) = &result.execute_result else {
                return None;
            };
            rewards += result.rewards;
        }
        Some(rewards)
    }
}

impl<DB, C> Scheduler<DB, C>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync,
    C: AsyncCommit + Send,
{
    pub fn new(
        spec_id: SpecId,
        env: Env,
        txs: Arc<Vec<TxEnv>>,
        db: DB,
        commiter: C,
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
            db,
            commiter: Mutex::new(commiter),
            cache: CachedStorageData::new(),
            tx_states: (0..num_txs).map(|_| RwLock::new(TxState::default())).collect(),
            tx_results: (0..num_txs).map(|_| Mutex::new(None)).collect(),
            tx_dependency: Mutex::new(tx_dependency),
            mv_memory: MVMemory::new(),
            finality_idx: AtomicUsize::new(0),
            validation_idx: AtomicUsize::new(0),
            execution_idx: AtomicUsize::new(0),
            abort: AtomicBool::new(false),
            metrics: ExecuteMetricsCollector::default(),
        }
    }

    fn async_commit(&self, task_queue: &LockFreeQueue<Task>) {
        let mut start = Instant::now();
        let mut num_commit = 0;
        while !self.abort.load(Ordering::Relaxed) && num_commit < self.block_size {
            let finality_idx = self.finality_idx.load(Ordering::Relaxed);
            if num_commit == finality_idx {
                thread::yield_now();
            } else {
                while num_commit < finality_idx {
                    let result =
                        self.tx_results[num_commit].lock().as_ref().unwrap().execute_result.clone();
                    let Ok(result) = result else { panic!("Commit error tx: {}", num_commit) };
                    self.commiter.lock().commit(result, &self.cache);
                    num_commit += 1;
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
                    self.tx_states.iter().map(|s| s.read().status.clone()).enumerate().collect();
                println!("transaction status: {:?}", status);
                self.tx_dependency.lock().print();
                println!("task queue size: {}", task_queue.len());
            }
        }
    }

    pub fn with_commiter<F, R>(&self, func: F) -> R
    where
        F: FnOnce(&mut C) -> R,
    {
        let mut commiter = self.commiter.lock();
        func(&mut *commiter)
    }

    pub fn parallel_execute(
        &self,
        concurrency_level: Option<usize>,
    ) -> Result<(), EVMError<DB::Error>> {
        self.metrics.total_tx_cnt.store(self.block_size, Ordering::Relaxed);
        let concurrency_level = concurrency_level.unwrap_or(*CONCURRENT_LEVEL);
        let task_queue = LockFreeQueue::new(concurrency_level * 4);
        thread::scope(|scope| {
            scope.spawn(|| {
                self.async_commit(&task_queue);
            });
            scope.spawn(|| {
                self.assign_tasks(&task_queue);
            });
            for _ in 0..concurrency_level {
                scope.spawn(|| {
                    let mut cache_db = CacheDB::new(
                        self.env.block.coinbase,
                        &self.db,
                        &self.cache,
                        &self.mv_memory,
                        &self,
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
        self.metrics.report();
        if self.abort.load(Ordering::Relaxed) {
            let result = self.tx_results[self.finality_idx.load(Ordering::Relaxed)].lock();
            if let Some(result) = result.as_ref() {
                if let Err(e) = &result.execute_result {
                    return Err(e.clone());
                }
            }
            panic!("Wrong abort transaction")
        } else {
            Ok(())
        }
    }

    fn execute<RA>(&self, evm: &mut Evm<'_, (), &mut CacheDB<DB, RA>>, tx_version: TxVersion)
    where
        RA: RewardsAccumulator,
    {
        self.metrics.execution_cnt.fetch_add(1, Ordering::Relaxed);
        let finality_idx = self.finality_idx.load(Ordering::Relaxed);
        let TxVersion { txid, incarnation } = tx_version;
        evm.db_mut().reset_state(TxVersion::new(txid, incarnation));
        *evm.tx_mut() = self.txs[txid].clone();
        let result = evm.transact_lazy_reward();

        match result {
            Ok(result_and_state) => {
                // only the miner involved in transaction should accumulate the rewards of finality
                // txs return true if the tx doesn't visit the miner account
                let rewards_accumulated = evm.db().rewards_accumulated();
                let conflict = !rewards_accumulated || evm.db().visit_estimate();
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
                    let mut tx_state = self.tx_states[txid].write();
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
                    if !rewards_accumulated {
                        self.metrics.conflict_by_miner.fetch_add(1, Ordering::Relaxed);
                        // Add all previous transactions as dependencies if miner doesn't accumulate
                        // the rewards
                        self.tx_dependency.lock().add(txid, Some(txid - 1));
                    } else {
                        self.metrics.conflict_by_estimate.fetch_add(1, Ordering::Relaxed);
                        let dep_tx = self.generate_dependent_tx(txid, &read_set);
                        self.tx_dependency.lock().add(txid, dep_tx);
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
                let mut read_set = evm.db_mut().take_read_set();
                let mut write_set = HashSet::new();
                read_set
                    .entry(LocationAndType::Basic(evm.tx().caller))
                    .or_insert(ReadVersion::Storage);
                if let TxKind::Call(to) = evm.tx().transact_to {
                    read_set.entry(LocationAndType::Basic(to)).or_insert(ReadVersion::Storage);
                }

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_mut() {
                    write_set = std::mem::take(&mut last_result.write_set);
                    for location in write_set.iter() {
                        if let Some(mut written_transactions) = self.mv_memory.get_mut(location) {
                            if let Some(entry) = written_transactions.get_mut(&txid) {
                                entry.estimate = true;
                            }
                        }
                    }
                }

                // update transaction status
                {
                    let mut tx_state = self.tx_states[txid].write();
                    if tx_state.incarnation != incarnation {
                        panic!("Inconsistent incarnation when execution");
                    }
                    tx_state.status = TransactionStatus::Conflict;
                }
                {
                    let dep_tx = self.generate_dependent_tx(txid, &read_set);
                    self.tx_dependency.lock().add(txid, dep_tx);
                }
                *last_result =
                    Some(TransactionResult { read_set, write_set, execute_result: Err(e) });
                if finality_idx == txid {
                    self.abort.store(true, Ordering::Relaxed);
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
                    if let ReadVersion::MvMemory(version) = version {
                        if version.txid != previous_id ||
                            version.incarnation != latest_version.incarnation
                        {
                            conflict = true;
                            break;
                        }
                    } else {
                        conflict = true;
                        break;
                    }
                } else if !matches!(version, ReadVersion::Storage) {
                    conflict = true;
                    break;
                }
            } else if !matches!(version, ReadVersion::Storage) {
                conflict = true;
                break;
            }
        }

        // update transaction status
        {
            let mut tx_state = self.tx_states[txid].write();
            if tx_state.incarnation != incarnation {
                panic!("Inconsistent incarnation when validating");
            }
            tx_state.status =
                if conflict { TransactionStatus::Conflict } else { TransactionStatus::Unconfirmed };
            tx_state.dependency = dependency.clone();
        }

        if conflict {
            self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
            self.metrics.conflict_by_version.fetch_add(1, Ordering::Relaxed);
            // mark write set as estimate
            self.mark_estimate(txid, incarnation, &result.write_set);
            // update dependency
            let dep_tx = match dependency {
                None => None,
                Some(dep) => {
                    if dep >= self.finality_idx.load(Ordering::Relaxed) {
                        Some(dep)
                    } else {
                        None
                    }
                }
            };
            self.tx_dependency.lock().add(txid, dep_tx);
        }
    }

    fn mark_estimate(&self, txid: TxId, incarnation: usize, write_set: &HashSet<LocationAndType>) {
        for location in write_set {
            if let Some(mut written_transactions) = self.mv_memory.get_mut(location) {
                if let Some(entry) = written_transactions.get_mut(&txid) {
                    if entry.incarnation == incarnation {
                        entry.estimate = true;
                    }
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
            if finality_idx < validation_idx {
                let mut tx_dependency = self.tx_dependency.lock();
                while finality_idx < validation_idx {
                    let mut tx = self.tx_states[finality_idx].write();
                    match tx.status {
                        TransactionStatus::Unconfirmed => {
                            tx.status = TransactionStatus::Finality;
                            tx_dependency.commit(finality_idx);
                            finality_idx += 1;
                            if tx.dependency.is_none() {
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
                self.finality_idx.store(finality_idx, Ordering::Release);
            }

            // Prior to submit validation task
            let mut num_tasks = 0;
            let num_validation = task_queue.capacity() - task_queue.len();
            while validation_idx < execution_idx && num_tasks < num_validation {
                let mut tx = self.tx_states[validation_idx].write();
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
                    let mut tx = self.tx_states[execute_id].write();
                    if !matches!(
                        tx.status,
                        TransactionStatus::Initial | TransactionStatus::Conflict
                    ) {
                        self.metrics.useless_dependent_update.fetch_add(1, Ordering::Relaxed);
                        tx_dependency.commit(execute_id);
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
