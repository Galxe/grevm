use crate::storage::{PartitionDB, SchedulerDB};
use crate::{GrevmResult, LocationAndType, PartitionId, ResultAndTransition, TxId};
use revm::primitives::{Address, Env, SpecId, TxEnv, TxKind};
use revm::{DatabaseRef, EvmBuilder};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct PartitionMetrics {
    pub execute_time: Duration,
    pub reusable_tx_cnt: u64,
}

/// Add some binary search methods for ordered vectors
pub(crate) trait OrderedVectorExt<E> {
    fn has(&self, element: &E) -> bool;

    fn index(&self, element: &E) -> usize;
}

impl<E: Ord> OrderedVectorExt<E> for Vec<E> {
    fn has(&self, element: &E) -> bool {
        self.binary_search(element).is_ok()
    }

    fn index(&self, element: &E) -> usize {
        self.binary_search(element).unwrap()
    }
}

#[derive(Clone)]
pub(crate) struct PreUnconfirmedContext {
    pub read_set: HashSet<LocationAndType>,
    pub write_set: HashSet<LocationAndType>,
    pub execute_state: ResultAndTransition,
}

/// Record some status from the previous round,
/// which can accelerate the execution of the current round
#[derive(Default)]
pub(crate) struct PreRoundContext {
    pub pre_unconfirmed_txs: HashMap<TxId, PreUnconfirmedContext>,
}

impl PreRoundContext {
    pub(crate) fn take_pre_unconfirmed_ctx(
        &mut self,
        txid: &TxId,
    ) -> Option<PreUnconfirmedContext> {
        self.pre_unconfirmed_txs.remove(txid)
    }
}

pub(crate) struct PartitionExecutor<DB>
where
    DB: DatabaseRef,
{
    spec_id: SpecId,
    env: Env,
    coinbase: Address,
    pub partition_id: PartitionId,

    pub partition_db: PartitionDB<DB>,

    txs: Arc<Vec<TxEnv>>,
    pub assigned_txs: Vec<TxId>,
    pub read_set: Vec<HashSet<LocationAndType>>,
    pub write_set: Vec<HashSet<LocationAndType>>,

    pub execute_results: Vec<GrevmResult<DB::Error>>,

    pub conflict_txs: Vec<TxId>,
    pub unconfirmed_txs: Vec<(TxId, usize)>, // TxId and index
    pub tx_dependency: Vec<BTreeSet<TxId>>,

    pub pre_round_ctx: Option<PreRoundContext>,

    pub metrics: PartitionMetrics,
}

impl<DB> PartitionExecutor<DB>
where
    DB: DatabaseRef,
{
    pub(crate) fn new(
        spec_id: SpecId,
        partition_id: PartitionId,
        env: Env,
        scheduler_db: Arc<SchedulerDB<DB>>,
        txs: Arc<Vec<TxEnv>>,
        assigned_txs: Vec<TxId>,
    ) -> Self {
        let coinbase = env.block.coinbase.clone();
        let partition_db = PartitionDB::new(coinbase.clone(), scheduler_db);
        Self {
            spec_id,
            env,
            coinbase,
            partition_id,
            partition_db,
            txs,
            assigned_txs,
            read_set: vec![],
            write_set: vec![],
            execute_results: vec![],
            conflict_txs: vec![],
            unconfirmed_txs: vec![],
            tx_dependency: vec![],
            pre_round_ctx: None,
            metrics: Default::default(),
        }
    }

    pub(crate) fn set_pre_round_ctx(&mut self, pre_round_context: Option<PreRoundContext>) {
        self.pre_round_ctx = pre_round_context;
    }

    pub(crate) fn execute(&mut self) {
        let start = Instant::now();
        let mut update_write_set: HashSet<LocationAndType> = HashSet::new();
        let mut evm = EvmBuilder::default()
            .with_db(&mut self.partition_db)
            .with_spec_id(self.spec_id)
            .with_env(Box::new(self.env.clone()))
            .build();
        for txid in &self.assigned_txs {
            let mut miner_involved = false;
            if let Some(tx) = self.txs.get(*txid) {
                *evm.tx_mut() = tx.clone();
                // If the transaction includes the miner’s address,
                // we need to record the miner’s address in both the write and read sets,
                // and fully track the updates to the miner’s account.
                if self.coinbase == tx.caller {
                    miner_involved = true;
                }
                if let TxKind::Call(to) = tx.transact_to {
                    if self.coinbase == to {
                        miner_involved = true;
                    }
                }
            } else {
                panic!("Wrong transactions ID");
            }
            let mut should_rerun = true;
            if let Some(ctx) = self.pre_round_ctx.as_mut() {
                if let Some(PreUnconfirmedContext { read_set, write_set, execute_state }) =
                    ctx.take_pre_unconfirmed_ctx(txid)
                {
                    // The unconfirmed transactions from the previous round may not require repeated execution.
                    // The verification process is as follows:
                    // 1. create a write set(update_write_set) to store write_set of previous conflict tx
                    // 2. if an unconfirmed tx rerun, store write_set to update_write_set
                    // 3. when running an unconfirmed tx, take it's pre-round read_set to join with update_write_set
                    // 4. if the intersection is None, the unconfirmed tx no need to rerun, otherwise rerun
                    if read_set.is_disjoint(&update_write_set) {
                        self.read_set.push(read_set);
                        self.write_set.push(write_set);
                        // All accounts should be present inside cache.
                        for address in execute_state.transition.keys() {
                            // FIXME(gravity): error handling
                            let _ = evm.db_mut().load_cache_account(*address);
                        }
                        evm.db_mut().temporary_commit(&execute_state.transition);
                        self.execute_results.push(Ok(execute_state));
                        should_rerun = false;
                        self.metrics.reusable_tx_cnt += 1;
                    }
                }
            }
            if should_rerun {
                evm.db_mut().miner_involved = miner_involved;
                let result = evm.transact();
                match result {
                    Ok(mut result_and_state) => {
                        // update read set
                        self.read_set.push(evm.db_mut().take_read_set());
                        // update write set
                        let (write_set, rewards) =
                            evm.db().generate_write_set(&result_and_state.state);
                        if self.pre_round_ctx.is_some() {
                            update_write_set.extend(write_set.clone().into_iter());
                        }
                        self.write_set.push(write_set);
                        if rewards.is_some() {
                            // remove miner's state if we handle rewards separately
                            result_and_state.state.remove(&self.coinbase);
                        }
                        // temporary commit to cache_db, to make use the remaining txs can read the updated data
                        evm.db_mut().temporary_commit(&result_and_state.state);
                        self.execute_results.push(Ok(ResultAndTransition {
                            result: Some(result_and_state.result),
                            transition: result_and_state.state,
                            rewards: rewards.unwrap_or(0),
                        }));
                    }
                    Err(err) => {
                        // Due to parallel execution, transactions may fail (such as dependent transfers not yet received),
                        // so the transaction failure does not result in block failure.
                        // Only the failure of transactions that in FINALITY state will result in block failure.
                        // During verification, failed transactions are conflict state

                        // update read set
                        let mut read_set = evm.db_mut().take_read_set();
                        // update write set with the caller and transact_to
                        let mut write_set = HashSet::new();
                        read_set.insert(LocationAndType::Basic(evm.tx().caller));
                        write_set.insert(LocationAndType::Basic(evm.tx().caller));
                        if let TxKind::Call(to) = evm.tx().transact_to {
                            read_set.insert(LocationAndType::Basic(to));
                            write_set.insert(LocationAndType::Basic(to));
                        }
                        self.read_set.push(read_set);
                        self.write_set.push(write_set);
                        self.execute_results.push(Err(err));
                    }
                }
            }
        }
        assert_eq!(self.assigned_txs.len(), self.execute_results.len());
        self.metrics.execute_time = start.elapsed();
    }
}
