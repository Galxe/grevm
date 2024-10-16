use fastrace::Span;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::DerefMut;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::hint::ParallelExecutionHints;
use crate::partition::{
    OrderedVectorExt, PartitionExecutor, PreRoundContext, PreUnconfirmedContext,
};
use crate::storage::{SchedulerDB, StateCache};
use crate::tx_dependency::{DependentTxsVec, TxDependency};
use crate::{
    fork_join_util, GrevmError, LocationAndType, TxId, CPU_CORES, GREVM_RUNTIME, MAX_NUM_ROUND,
};

use metrics::{counter, gauge};
use revm::db::states::bundle_state::BundleRetention;
use revm::db::BundleState;
use revm::primitives::{
    AccountInfo, Address, Bytecode, EVMError, Env, ExecutionResult, SpecId, TxEnv, B256, U256,
};
use revm::{DatabaseRef, EvmBuilder};

pub struct ExecuteMetrics {
    /// Number of times parallel execution is called.
    parallel_execute_calls: metrics::Counter,
    /// Number of times sequential execution is called.
    sequential_execute_calls: metrics::Counter,

    /// Total number of transactions.
    total_tx_cnt: metrics::Counter,
    /// Number of transactions executed in parallel.
    parallel_tx_cnt: metrics::Counter,
    /// Number of transactions executed sequentially.
    sequential_tx_cnt: metrics::Counter,
    /// Number of transactions that encountered conflicts.
    conflict_tx_cnt: metrics::Counter,
    /// Number of transactions that reached finality.
    finality_tx_cnt: metrics::Counter,
    /// Number of transactions that are unconfirmed.
    unconfirmed_tx_cnt: metrics::Counter,
    /// Number of reusable transactions.
    reusable_tx_cnt: metrics::Counter,

    /// Number of concurrent partitions.
    concurrent_partition_num: metrics::Gauge,
    /// Execution time difference between partitions(in nanoseconds).
    partition_et_diff: metrics::Gauge,
    /// Number of transactions difference between partitions.
    partition_tx_diff: metrics::Gauge,

    /// Time taken to parse execution hints(in nanoseconds).
    parse_hints_time: metrics::Counter,
    /// Time taken to partition transactions(in nanoseconds).
    partition_tx_time: metrics::Counter,
    /// Time taken to validate transactions(in nanoseconds).
    validate_time: metrics::Counter,
    /// Size of the write set.
    write_set_size: metrics::Counter,
}

impl Default for ExecuteMetrics {
    fn default() -> Self {
        Self {
            parallel_execute_calls: counter!("grevm.parallel_round_calls"),
            sequential_execute_calls: counter!("grevm.sequential_execute_calls"),
            total_tx_cnt: counter!("grevm.total_tx_cnt"),
            parallel_tx_cnt: counter!("grevm.parallel_tx_cnt"),
            sequential_tx_cnt: counter!("grevm.sequential_tx_cnt"),
            finality_tx_cnt: counter!("grevm.finality_tx_cnt"),
            conflict_tx_cnt: counter!("grevm.conflict_tx_cnt"),
            unconfirmed_tx_cnt: counter!("grevm.unconfirmed_tx_cnt"),
            reusable_tx_cnt: counter!("grevm.reusable_tx_cnt"),
            concurrent_partition_num: gauge!("grevm.concurrent_partition_num"),
            partition_et_diff: gauge!("grevm.partition_execution_time_diff"),
            partition_tx_diff: gauge!("grevm.partition_num_tx_diff"),
            parse_hints_time: counter!("grevm.parse_hints_time"),
            partition_tx_time: counter!("grevm.partition_tx_time"),
            validate_time: counter!("grevm.validate_time"),
            write_set_size: counter!("grevm.write_set_size"),
        }
    }
}

pub struct ExecuteOutput {
    /// The changed state of the block after execution.
    pub state: BundleState,
    /// All the results of the transactions in the block.
    pub results: Vec<ExecutionResult>,
}

pub struct GrevmScheduler<DB>
where
    DB: DatabaseRef,
{
    // The number of transactions in the tx batch size.
    tx_batch_size: usize,

    spec_id: SpecId,
    env: Env,
    coinbase: Address,
    txs: Arc<Vec<TxEnv>>,

    database: Arc<SchedulerDB<DB>>,

    parallel_execution_hints: ParallelExecutionHints,

    tx_dependencies: TxDependency,

    // number of partitions. maybe larger in the first round to increase concurrence
    num_partitions: usize,
    // assigned txs ID for each partition
    partitioned_txs: Vec<Vec<TxId>>,
    partition_executors: Vec<Arc<RwLock<PartitionExecutor<DB>>>>,

    num_finality_txs: usize,

    // update after each round
    pre_unconfirmed_txs: BTreeMap<TxId, PreUnconfirmedContext>,

    results: Vec<ExecutionResult>,

    metrics: ExecuteMetrics,
}

pub struct DatabaseWrapper<Error>(Arc<dyn DatabaseRef<Error = Error> + Send + Sync + 'static>);

impl<Error> DatabaseRef for DatabaseWrapper<Error> {
    type Error = Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.0.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.0.block_hash_ref(number)
    }
}

/// Creates a new GrevmScheduler instance.
pub fn new_grevm_scheduler<DB>(
    spec_id: SpecId,
    env: Env,
    db: DB,
    txs: Vec<TxEnv>,
) -> GrevmScheduler<DatabaseWrapper<DB::Error>>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    // FIXME(gravity_nekomoto): use unsafe to bypass the 'static constraint of `tokio::spawn`.
    // We can be confident that the spawned task will be joined before the members in `db` are
    // dropped.
    let db = unsafe {
        let boxed: Arc<dyn DatabaseRef<Error = DB::Error>> = Arc::new(db);
        std::mem::transmute::<
            Arc<dyn DatabaseRef<Error = DB::Error>>,
            Arc<dyn DatabaseRef<Error = DB::Error> + Send + Sync + 'static>,
        >(boxed)
    };
    let db: DatabaseWrapper<DB::Error> = DatabaseWrapper(db);
    GrevmScheduler::new(spec_id, env, db, txs)
}

impl<DB> GrevmScheduler<DB>
where
    DB: DatabaseRef + Send + Sync + 'static,
    DB::Error: Send + Sync + Clone,
{
    pub fn new(spec_id: SpecId, env: Env, db: DB, txs: Vec<TxEnv>) -> Self {
        let coinbase = env.block.coinbase.clone();
        let start = Instant::now();
        let parallel_execution_hints = ParallelExecutionHints::new(&txs);
        let tx_dependencies = TxDependency::new(&parallel_execution_hints);
        let elapsed = start.elapsed().as_nanos();
        let num_partitions = *CPU_CORES * 2 + 1; // 2 * cpu + 1 for initial partition number
        let mut s = Self {
            tx_batch_size: txs.len(),
            spec_id,
            env,
            coinbase,
            txs: Arc::new(txs),
            database: Arc::new(SchedulerDB::new(db)),
            parallel_execution_hints,
            tx_dependencies,
            num_partitions,
            partitioned_txs: vec![],
            partition_executors: vec![],
            num_finality_txs: 0,
            pre_unconfirmed_txs: BTreeMap::new(),
            results: Vec::new(),
            metrics: Default::default(),
        };
        s.metrics.parse_hints_time.increment(elapsed as u64);
        s
    }

    #[fastrace::trace]
    pub(crate) fn partition_transactions(&mut self) {
        // compute and assign partitioned_txs
        let start = Instant::now();
        self.partitioned_txs = self.tx_dependencies.fetch_best_partitions(self.num_partitions);
        self.num_partitions = self.partitioned_txs.len();
        let mut max = 0;
        let mut min = self.txs.len();
        for partition in &self.partitioned_txs {
            if partition.len() > max {
                max = partition.len();
            }
            if partition.len() < min {
                min = partition.len();
            }
        }
        self.metrics.partition_tx_diff.set((max - min) as f64);
        self.metrics.concurrent_partition_num.set(self.num_partitions as f64);
        self.metrics.partition_tx_time.increment(start.elapsed().as_nanos() as u64);
    }

    pub fn set_num_partitions(&mut self, num_partitions: usize) {
        self.num_partitions = num_partitions;
    }

    pub fn clean_dependency(&mut self) {
        self.tx_dependencies.clean_dependency();
    }

    fn generate_partition_pre_context(&mut self) {
        fork_join_util(self.num_partitions, Some(self.num_partitions), |_, _, part| {
            let mut pre_unconfirmed_txs = HashMap::new();
            for txid in &self.partitioned_txs[part] {
                if let Some(ctx) = self.pre_unconfirmed_txs.get(txid) {
                    pre_unconfirmed_txs.insert(*txid, ctx.clone());
                }
            }
            if !pre_unconfirmed_txs.is_empty() {
                let mut executor = self.partition_executors[part].write().unwrap();
                executor.set_pre_round_ctx(Some(PreRoundContext { pre_unconfirmed_txs }));
            }
        });
        // released pre_unconfirmed_txs
        self.pre_unconfirmed_txs.clear();
    }

    #[fastrace::trace]
    fn round_execute(&mut self) -> Result<(), GrevmError<DB::Error>> {
        self.metrics.parallel_execute_calls.increment(1);
        self.partition_executors.clear();
        for partition_id in 0..self.num_partitions {
            let mut executor = PartitionExecutor::new(
                self.spec_id.clone(),
                partition_id,
                self.env.clone(),
                self.database.clone(),
                self.txs.clone(),
                self.partitioned_txs[partition_id].clone(),
            );
            self.partition_executors.push(Arc::new(RwLock::new(executor)));
        }
        self.generate_partition_pre_context();

        GREVM_RUNTIME.block_on(async {
            let mut tasks = vec![];
            for executor in &self.partition_executors {
                let executor = executor.clone();
                tasks.push(GREVM_RUNTIME.spawn(async move { executor.write().unwrap().execute() }));
            }
            futures::future::join_all(tasks).await;
        });

        self.validate_transactions()
    }

    // merge write set after each round
    #[fastrace::trace]
    fn merge_write_set(&mut self) -> HashMap<LocationAndType, BTreeSet<TxId>> {
        let mut merged_write_set: HashMap<LocationAndType, BTreeSet<TxId>> = HashMap::new();
        let mut write_set_size = 0;
        for executor in &self.partition_executors {
            let executor = executor.read().unwrap();
            for (txid, ws) in executor.assigned_txs.iter().zip(executor.write_set.iter()) {
                write_set_size += ws.len();
                for location in ws {
                    merged_write_set.entry(location.clone()).or_default().insert(*txid);
                }
            }
        }
        self.metrics.write_set_size.increment(write_set_size as u64);
        merged_write_set
    }

    /// When validating the transaction status, the dependency relationship was updated.
    /// But there are some transactions that have entered the finality state,
    /// and there is no need to record the dependency and dependent relationships of these transactions.
    /// Thus achieving the purpose of pruning.
    #[fastrace::trace]
    fn update_and_pruning_dependency(&mut self) {
        let num_finality_txs = self.num_finality_txs;
        if num_finality_txs == self.txs.len() {
            return;
        }
        let mut new_dependency: Vec<DependentTxsVec> =
            vec![DependentTxsVec::new(); self.txs.len() - num_finality_txs];
        for executor in &self.partition_executors {
            let executor = executor.read().unwrap();
            for (txid, dep) in executor.assigned_txs.iter().zip(executor.tx_dependency.iter()) {
                if *txid >= num_finality_txs {
                    // pruning the tx that is finality state
                    new_dependency[*txid - num_finality_txs] = dep
                        .clone()
                        .into_iter()
                        // pruning the dependent tx that is finality state
                        .filter(|dep_id| *dep_id >= num_finality_txs)
                        .collect();
                }
            }
        }
        self.tx_dependencies.update_tx_dependency(new_dependency, num_finality_txs);
    }

    /// verification of transaction state after each round
    /// Because after each round execution, the read-write set is no longer updated.
    /// We can check in parallel whether the read set is out of bounds.
    #[fastrace::trace]
    fn validate_transactions(&mut self) -> Result<(), GrevmError<DB::Error>> {
        let start = Instant::now();
        let span = Span::enter_with_local_parent("generate unconfirmed txs");

        GREVM_RUNTIME.block_on(async {
            let mut tasks = vec![];
            let merged_write_set = Arc::new(self.merge_write_set());
            for executor in &self.partition_executors {
                let executor = executor.clone();
                let merged_write_set = merged_write_set.clone();
                tasks.push(GREVM_RUNTIME.spawn(async move {
                    // Tx validate process:
                    // 1. Traverse the read set of each tx in each partition, and find the Range<TxId> less than tx in merged_write_set
                    // 2. conflict: 1) exist tx in Range<TxId> not belong to tx's partition; 2) exist tx in Range<TxId> is conflicted
                    let mut executor = executor.write().unwrap();
                    let executor = executor.deref_mut();

                    for (index, rs) in executor.read_set.iter().enumerate() {
                        let txid = executor.assigned_txs[index];
                        let mut conflict = executor.execute_results[index].is_err();
                        if conflict {
                            // Transactions that fail in partition executor are in conflict state
                            executor.conflict_txs.push(txid);
                        }
                        let mut updated_dependencies = BTreeSet::new();
                        for location in rs {
                            if let Some(written_txs) = merged_write_set.get(location) {
                                if let Some(previous_txid) = written_txs.range(..txid).next_back() {
                                    // update dependencies: previous_txid <- txid
                                    updated_dependencies.insert(*previous_txid);
                                    if !conflict
                                        && (!executor.assigned_txs.has(previous_txid)
                                            || executor.conflict_txs.has(previous_txid))
                                    {
                                        executor.conflict_txs.push(txid);
                                        conflict = true;
                                    }
                                }
                            }
                        }
                        executor.tx_dependency.push(updated_dependencies);
                        if !conflict {
                            executor.unconfirmed_txs.push((txid, index));
                        }
                    }
                    assert_eq!(
                        executor.unconfirmed_txs.len() + executor.conflict_txs.len(),
                        executor.assigned_txs.len()
                    );
                }));
            }
            futures::future::join_all(tasks).await;
        });
        drop(span);
        // find the continuous min txid
        let span = Span::enter_with_local_parent("find the continuous min txid");
        let mut unconfirmed_txs = BTreeMap::new();
        let mut min_execute_time = Duration::from_secs(u64::MAX);
        let mut max_execute_time = Duration::from_secs(0);
        for executor in &self.partition_executors {
            let mut executor = executor.write().unwrap();
            self.metrics.reusable_tx_cnt.increment(executor.metrics.reusable_tx_cnt);
            min_execute_time = min_execute_time.min(executor.metrics.execute_time);
            max_execute_time = max_execute_time.max(executor.metrics.execute_time);
            if executor.assigned_txs[0] == self.num_finality_txs {
                if let Err(err) = &executor.execute_results[0] {
                    return Err(GrevmError::EvmError(err.clone()));
                }
            }

            let partition_unconfirmed_txs = std::mem::take(&mut executor.unconfirmed_txs);
            let mut partition_execute_state = std::mem::take(&mut executor.execute_results);

            for (txid, index) in partition_unconfirmed_txs.into_iter() {
                match &mut partition_execute_state[index] {
                    Ok(state) => {
                        unconfirmed_txs.insert(
                            txid,
                            PreUnconfirmedContext {
                                read_set: std::mem::take(&mut executor.read_set[index]),
                                write_set: std::mem::take(&mut executor.write_set[index]),
                                execute_state: std::mem::take(state),
                            },
                        );
                    }
                    Err(_) => {
                        return Err(GrevmError::UnreachableError(String::from(
                            "Should take failed transaction as conflict",
                        )));
                    }
                }
            }
        }
        self.metrics.partition_et_diff.set((max_execute_time - min_execute_time).as_nanos() as f64);
        if let Some((min_txid, _)) = unconfirmed_txs.first_key_value() {
            if *min_txid != self.num_finality_txs {
                // Most likely, the execution of the smallest transaction failed in evm.transact()
                return Err(GrevmError::ExecutionError(String::from(
                    "Discontinuous finality transaction ID",
                )));
            }
        } else {
            return Err(GrevmError::ExecutionError(String::from(
                "Can't finalize a transaction in an execution round",
            )));
        }

        for txid in unconfirmed_txs.keys() {
            if *txid == self.num_finality_txs {
                self.num_finality_txs += 1;
            } else {
                break;
            }
        }

        drop(span);
        self.pre_unconfirmed_txs = unconfirmed_txs.split_off(&self.num_finality_txs);
        // Now `unconfirmed_txs` only contains the txs that are finality in this round
        let finality_txs = unconfirmed_txs;
        let conflict_tx_cnt =
            self.txs.len() - self.num_finality_txs - self.pre_unconfirmed_txs.len();
        self.metrics.conflict_tx_cnt.increment(conflict_tx_cnt as u64);
        self.metrics.unconfirmed_tx_cnt.increment(self.pre_unconfirmed_txs.len() as u64);
        self.metrics.finality_tx_cnt.increment(finality_txs.len() as u64);

        // update and pruning tx dependencies
        self.update_and_pruning_dependency();

        let partition_state: Vec<StateCache> = self
            .partition_executors
            .iter()
            .map(|executor| {
                let mut executor = executor.write().unwrap();
                let mut cache = std::mem::take(&mut executor.partition_db.cache);
                if !executor.partition_db.miner_involved {
                    // Remove miner account in partitions that only treat it as a reward account.
                    // We have preload it into scheduler state cache, and will handle rewards
                    // separately. Only when miner account participates in the transactions should
                    // it be merged into the scheduler state cache.
                    cache.accounts.remove(&self.coinbase);
                }
                cache
            })
            .collect();

        // MUST drop the `PartitionExecutor::scheduler_db` before get mut
        let span = Span::enter_with_local_parent("clear partition_executors");
        self.partition_executors.clear();
        drop(span);
        let database = Arc::get_mut(&mut self.database).unwrap();

        if self.num_finality_txs == self.txs.len() {
            // Merge partition state to scheduler directly if there are no conflicts in the entire
            // partition.
            let span = Span::enter_with_local_parent("directly merge accounts");
            for state in partition_state {
                database.cache.merge_accounts(state);
            }

            let mut rewards: u128 = 0;
            for ctx in finality_txs.into_values() {
                rewards += ctx.execute_state.rewards;
                self.results.push(ctx.execute_state.result.unwrap());
            }
            database
                .increment_balances(vec![(self.coinbase, rewards)])
                .map_err(|err| GrevmError::EvmError(EVMError::Database(err)))?;

            self.metrics.validate_time.increment(start.elapsed().as_nanos() as u64);
            return Ok(());
        }

        // Must merge loaded state to database cache before commit changes
        let span = Span::enter_with_local_parent("merge loaded state");
        for state in partition_state {
            database.cache.merge_loaded_state(state);
        }
        std::mem::drop(span);

        let span = Span::enter_with_local_parent("commit transition to scheduler db");
        let mut rewards: u128 = 0;
        for ctx in finality_txs.into_values() {
            rewards += ctx.execute_state.rewards;
            self.results.push(ctx.execute_state.result.unwrap());
            database.commit(&ctx.execute_state.transition);
        }
        // Each transaction updates three accounts: from, to, and coinbase.
        // If every tx updates the coinbase account, it will cause conflicts across all txs.
        // Therefore, we handle miner rewards separately. We don't record miner’s address in r/w set,
        // and track the rewards for the miner for each transaction separately.
        // The miner’s account is only updated after validation by SchedulerDB.increment_balances
        database
            .increment_balances(vec![(self.coinbase, rewards)])
            .map_err(|err| GrevmError::EvmError(EVMError::Database(err)))?;
        drop(span);

        self.metrics.validate_time.increment(start.elapsed().as_nanos() as u64);
        Ok(())
    }

    #[fastrace::trace]
    fn execute_remaining_sequential(&mut self) -> Result<(), GrevmError<DB::Error>> {
        self.metrics.sequential_execute_calls.increment(1);
        self.metrics.sequential_tx_cnt.increment((self.txs.len() - self.num_finality_txs) as u64);
        // MUST drop the `PartitionExecutor::scheduler_db` before get mut
        self.partition_executors.clear();
        let database = Arc::get_mut(&mut self.database).unwrap();
        let mut evm = EvmBuilder::default()
            .with_db(database)
            .with_spec_id(self.spec_id)
            .with_env(Box::new(self.env.clone()))
            .build();
        for txid in self.num_finality_txs..self.txs.len() {
            if let Some(tx) = self.txs.get(txid) {
                *evm.tx_mut() = tx.clone();
            } else {
                return Err(GrevmError::UnreachableError(String::from("Wrong transactions ID")));
            }
            match evm.transact() {
                Ok(result_and_state) => {
                    evm.db_mut().commit(&result_and_state.state);
                    self.results.push(result_and_state.result);
                }
                Err(err) => return Err(GrevmError::EvmError(err)),
            }
        }
        Ok(())
    }

    #[fastrace::trace]
    pub fn evm_execute(
        &mut self,
        force_sequential: Option<bool>,
    ) -> Result<ExecuteOutput, GrevmError<DB::Error>> {
        self.metrics.total_tx_cnt.increment(self.txs.len() as u64);
        let force_parallel = !force_sequential.unwrap_or(true); // adaptive false
        let force_sequential = force_sequential.unwrap_or(false); // adaptive false
        self.results.reserve(self.txs.len());

        let build_execute_output = |this: &mut Self| {
            // MUST drop the `PartitionExecutor::scheduler_db` before get mut
            this.partition_executors.clear();
            let database = Arc::get_mut(&mut this.database).unwrap();
            let state = database.create_bundle_state(BundleRetention::Reverts);
            ExecuteOutput { state, results: std::mem::take(&mut this.results) }
        };

        {
            // Preload coinbase account
            let db = Arc::get_mut(&mut self.database).unwrap();
            let _ = db
                .load_cache_account(self.coinbase)
                .map_err(|err| GrevmError::EvmError(EVMError::Database(err)))?;
        }

        if self.txs.len() < self.num_partitions && !force_parallel {
            self.execute_remaining_sequential()?;
            return Ok(build_execute_output(self));
        }

        if !force_sequential {
            let mut round = 0;
            while round < MAX_NUM_ROUND {
                if self.num_finality_txs < self.txs.len() {
                    self.partition_transactions();
                    if self.num_partitions == 1 && !force_parallel {
                        break;
                    }
                    round += 1;
                    self.round_execute()?;
                } else {
                    break;
                }
            }
            self.metrics.parallel_tx_cnt.increment(self.num_finality_txs as u64);
        }

        if self.num_finality_txs < self.txs.len() {
            self.execute_remaining_sequential()?;
        }

        Ok(build_execute_output(self))
    }

    pub fn adaptive_execute(mut self) -> Result<ExecuteOutput, GrevmError<DB::Error>> {
        self.evm_execute(None)
    }

    pub fn parallel_execute(mut self) -> Result<ExecuteOutput, GrevmError<DB::Error>> {
        self.evm_execute(Some(false))
    }

    pub fn sequential_execute(mut self) -> Result<ExecuteOutput, GrevmError<DB::Error>> {
        self.evm_execute(Some(true))
    }
}
