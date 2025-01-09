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
    cmp::{max, min},
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
    one_attempt_tx_cnt: metrics::Histogram,
    more_attempts_tx_cnt: metrics::Histogram,
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
            one_attempt_tx_cnt: histogram!("grevm.one_attempt_tx_cnt"),
            more_attempts_tx_cnt: histogram!("grevm.more_attempts_tx_cnt"),
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
    one_attempt_tx_cnt: AtomicUsize,
    more_attempts_tx_cnt: AtomicUsize,
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
            .one_attempt_tx_cnt
            .record(self.one_attempt_tx_cnt.load(Ordering::Relaxed) as f64);
        execute_metrics
            .more_attempts_tx_cnt
            .record(self.more_attempts_tx_cnt.load(Ordering::Relaxed) as f64);
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
    tx_states: Vec<Mutex<TxState>>,
    tx_results: Vec<Mutex<Option<TransactionResult<DB::Error>>>>,
    tx_dependency: TxDependency,

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
            if self.tx_states[to - 1].lock().status != TransactionStatus::Finality {
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
        let (tx_dependency, tx_states) = if with_hints {
            ParallelExecutionHints::new(txs.clone()).parse_hints()
        } else {
            (
                TxDependency::new(num_txs),
                (0..num_txs).map(|_| Mutex::new(TxState::default())).collect(),
            )
        };

        Self {
            spec_id,
            env,
            block_size: num_txs,
            txs,
            db,
            commiter: Mutex::new(commiter),
            cache: CachedStorageData::new(),
            tx_states,
            tx_results: (0..num_txs).map(|_| Mutex::new(None)).collect(),
            tx_dependency,
            mv_memory: MVMemory::new(),
            finality_idx: AtomicUsize::new(0),
            validation_idx: AtomicUsize::new(0),
            execution_idx: AtomicUsize::new(0),
            abort: AtomicBool::new(false),
            metrics: ExecuteMetricsCollector::default(),
        }
    }

    fn async_commit(&self) {
        let mut start = Instant::now();
        let mut num_commit = 0;
        while !self.abort.load(Ordering::Relaxed) && num_commit < self.block_size {
            if self.tx_states[num_commit].lock().status == TransactionStatus::Validated {
                let mut tx_state = self.tx_states[num_commit].lock();
                tx_state.status = TransactionStatus::Finality;
                if tx_state.has_dependency {
                    if tx_state.incarnation == 0 {
                        self.metrics.one_attempt_tx_cnt.fetch_add(1, Ordering::Relaxed);
                    } else if tx_state.incarnation > 1 {
                        self.metrics.more_attempts_tx_cnt.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    self.metrics.no_dependency_txs.fetch_add(1, Ordering::Relaxed);
                }

                let result =
                    self.tx_results[num_commit].lock().as_ref().unwrap().execute_result.clone();
                let Ok(result) = result else { panic!("Commit error tx: {}", num_commit) };
                self.commiter.lock().commit(result, &self.cache);

                self.finality_idx.fetch_add(1, Ordering::Relaxed);
                num_commit += 1;
            } else {
                thread::yield_now();
                if (Instant::now() - start).as_millis() > 8_000 {
                    start = Instant::now();
                    println!(
                        "stuck..., finality_idx: {}, validation_idx: {}, execution_idx: {}",
                        self.finality_idx.load(Ordering::Relaxed),
                        self.validation_idx.load(Ordering::Relaxed),
                        self.execution_idx.load(Ordering::Relaxed)
                    );
                    let status: Vec<(TxId, TransactionStatus)> = self
                        .tx_states
                        .iter()
                        .map(|s| s.lock().status.clone())
                        .enumerate()
                        .collect();
                    println!("transaction status: {:?}", status);
                    self.tx_dependency.print();
                }
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
        thread::scope(|scope| {
            scope.spawn(|| {
                self.async_commit();
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
                    loop {
                        let mut task = self.next();
                        if task.is_some() {
                            while !self.abort.load(Ordering::Relaxed) && task.is_some() {
                                task = match task.unwrap() {
                                    Task::Execution(tx_version) => {
                                        self.execute(&mut evm, tx_version)
                                    }
                                    Task::Validation(tx_version) => self.validate(tx_version),
                                }
                            }
                        } else {
                            break;
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

    fn update_dependency(&self, txid: TxId) {
        for affect_id in self.tx_dependency.remove(txid) {
            let mut affect = self.tx_states[affect_id].lock();
            if affect.status == TransactionStatus::Aborting && self.tx_dependency.is_read(affect_id)
            {
                affect.incarnation += 1;
                affect.status = TransactionStatus::ReadyToExecute;
                self.execution_idx.fetch_min(affect_id, Ordering::Relaxed);
            }
        }
    }

    fn reset_validation_idx(&self, reset_idx: TxId) {
        if self.validation_idx.fetch_min(reset_idx, Ordering::Relaxed) > reset_idx {
            self.metrics.reset_validation_idx_cnt.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn finish_execution(
        &self,
        tx_version: TxVersion,
        conflict: bool,
        blocking_txs: HashSet<TxId>,
        write_new_locations: bool,
    ) -> Option<Task> {
        let TxVersion { txid, incarnation } = tx_version;
        let mut tx_state = self.tx_states[txid].lock();
        if tx_state.incarnation != incarnation {
            panic!("Inconsistent incarnation when execution");
        }

        if conflict {
            self.reset_validation_idx(txid);
            if self.dependent_estimate_txs(txid, &blocking_txs) {
                tx_state.status = TransactionStatus::Aborting;
                None
            } else {
                tx_state.incarnation += 1;
                Some(Task::Execution(TxVersion::new(txid, tx_state.incarnation)))
            }
        } else {
            self.update_dependency(txid);
            if txid < self.validation_idx.load(Ordering::Relaxed) {
                if write_new_locations {
                    // Use `write_new_locations` to determine whether to revalidate subsequent
                    // transactions
                    self.reset_validation_idx(txid + 1);
                }
                tx_state.status = TransactionStatus::Validating;
                Some(Task::Validation(TxVersion::new(txid, incarnation)))
            } else {
                tx_state.status = TransactionStatus::Executed;
                None
            }
        }
    }

    fn execute<RA>(
        &self,
        evm: &mut Evm<'_, (), &mut CacheDB<DB, RA>>,
        tx_version: TxVersion,
    ) -> Option<Task>
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
                let mut blocking_txs = evm.db_mut().take_estimate_txs();
                let conflict = !rewards_accumulated || !blocking_txs.is_empty();
                let read_set = evm.db_mut().take_read_set();
                let write_set = evm.db().update_mv_memory(&result_and_state.state, conflict);
                let mut write_new_locations = false;

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
                } else if !write_set.is_empty() {
                    write_new_locations = true;
                }
                *last_result = Some(TransactionResult {
                    read_set,
                    write_set,
                    execute_result: Ok(result_and_state),
                });

                if conflict {
                    self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
                    if !rewards_accumulated {
                        self.metrics.conflict_by_miner.fetch_add(1, Ordering::Relaxed);
                        blocking_txs.insert(txid - 1);
                    } else {
                        self.metrics.conflict_by_estimate.fetch_add(1, Ordering::Relaxed);
                    }
                }
                self.finish_execution(
                    TxVersion::new(txid, incarnation),
                    conflict,
                    blocking_txs,
                    write_new_locations,
                )
            }
            Err(e) => {
                self.metrics.conflict_cnt.fetch_add(1, Ordering::Relaxed);
                self.metrics.conflict_by_error.fetch_add(1, Ordering::Relaxed);
                let mut write_set = HashSet::new();

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
                *last_result = Some(TransactionResult {
                    read_set: Default::default(),
                    write_set,
                    execute_result: Err(e),
                });

                // update transaction status
                if finality_idx == txid {
                    self.abort.store(true, Ordering::Relaxed);
                }
                let mut blocking_txs = HashSet::new();
                if txid > 0 {
                    blocking_txs.insert(txid - 1);
                }
                self.finish_execution(TxVersion::new(txid, incarnation), true, blocking_txs, true)
            }
        }
    }

    fn validate(&self, tx_version: TxVersion) -> Option<Task> {
        let TxVersion { txid, incarnation } = tx_version;
        let mut tx_state = self.tx_states[txid].lock();
        if tx_state.incarnation != incarnation {
            panic!("Inconsistent incarnation when validating");
        }
        self.metrics.validation_cnt.fetch_add(1, Ordering::Relaxed);
        // check the read version of read set
        let mut conflict = false;
        let tx_result = self.tx_results[txid].lock();
        let Some(result) = tx_result.as_ref() else {
            panic!("No result when validating");
        };
        if let Err(_) = &result.execute_result {
            panic!("Error transaction should take as conflict before validating");
        }

        let mut has_dependency = false;
        let mut dependency = HashSet::new();
        for (location, version) in result.read_set.iter() {
            if let Some(written_transactions) = self.mv_memory.get(location) {
                if let Some((&previous_id, latest_version)) =
                    written_transactions.range(..txid).next_back()
                {
                    has_dependency = true;
                    if latest_version.estimate {
                        conflict = true;
                        dependency.insert(previous_id);
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
            self.mark_estimate(txid, incarnation, &result.write_set);
        }
        // update transaction status
        tx_state.has_dependency = has_dependency;
        if conflict {
            self.reset_validation_idx(txid);
            // update dependency
            if self.dependent_estimate_txs(txid, &dependency) {
                tx_state.status = TransactionStatus::Aborting;
            } else {
                tx_state.incarnation += 1;
                tx_state.status = TransactionStatus::Executing;
                return Some(Task::Execution(TxVersion::new(txid, tx_state.incarnation)));
            }
        } else {
            self.update_dependency(txid);
            tx_state.status = TransactionStatus::Validated;
        }
        None
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

    fn dependent_estimate_txs(&self, txid: TxId, visit_estimate_txs: &HashSet<TxId>) -> bool {
        if !visit_estimate_txs.is_empty() {
            let mut largest_dep = 0;
            for &dep_id in visit_estimate_txs.iter() {
                if dep_id > largest_dep {
                    largest_dep = dep_id;
                }
            }
            let tx_state = self.tx_states[largest_dep].lock();
            if !matches!(
                tx_state.status,
                TransactionStatus::Executed |
                    TransactionStatus::Validated |
                    TransactionStatus::Finality
            ) {
                self.tx_dependency.add(txid, largest_dep);
                return true;
            }
        }
        false
    }

    fn try_execute(&self, txid: TxId) -> Option<TxVersion> {
        if txid < self.block_size {
            let mut tx = self.tx_states[txid].lock();
            if tx.status == TransactionStatus::ReadyToExecute {
                tx.status = TransactionStatus::Executing;
                return Some(TxVersion::new(txid, tx.incarnation));
            }
        }
        None
    }

    pub fn next(&self) -> Option<Task> {
        while self.finality_idx.load(Ordering::Relaxed) < self.block_size &&
            !self.abort.load(Ordering::Relaxed)
        {
            let execution_idx = self.execution_idx.load(Ordering::Relaxed);
            let validation_idx = self.validation_idx.load(Ordering::Relaxed);
            if execution_idx >= self.block_size && validation_idx >= self.block_size {
                thread::yield_now();
                continue;
            }

            // Prior to submit validation task
            if validation_idx < execution_idx {
                let txid = self.validation_idx.fetch_add(1, Ordering::Relaxed);
                if txid < self.block_size {
                    let mut tx = self.tx_states[txid].lock();
                    match tx.status {
                        TransactionStatus::ReadyToExecute => {
                            tx.status = TransactionStatus::Executing;
                            return Some(Task::Execution(TxVersion::new(txid, tx.incarnation)));
                        }
                        TransactionStatus::Executed | TransactionStatus::Validated => {
                            tx.status = TransactionStatus::Validating;
                            return Some(Task::Validation(TxVersion::new(txid, tx.incarnation)));
                        }
                        TransactionStatus::Aborting => {
                            continue;
                        }
                        _ => {}
                    }
                }
            }

            // Submit execution task
            if let Some(tx_version) =
                self.try_execute(self.execution_idx.fetch_add(1, Ordering::Relaxed))
            {
                return Some(Task::Execution(tx_version));
            }
        }
        None
    }
}
