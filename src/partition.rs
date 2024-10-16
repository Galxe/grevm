use crate::storage::{PartitionDB, SchedulerDB};
use crate::{
    LocationAndType, PartitionId, ResultAndTransition, SharedTxStates, TransactionStatus, TxId,
    TxState,
};
use revm::primitives::{Address, EVMError, Env, ResultAndState, SpecId, TxEnv, TxKind};
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

pub(crate) struct PartitionExecutor<DB>
where
    DB: DatabaseRef,
{
    spec_id: SpecId,
    env: Env,
    coinbase: Address,
    partition_id: PartitionId,
    tx_states: SharedTxStates,
    txs: Arc<Vec<TxEnv>>,

    pub partition_db: PartitionDB<DB>,
    pub assigned_txs: Vec<TxId>,

    pub error_txs: HashMap<TxId, EVMError<DB::Error>>,

    pub tx_dependency: Vec<BTreeSet<TxId>>,

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
        tx_states: SharedTxStates,
        assigned_txs: Vec<TxId>,
    ) -> Self {
        let coinbase = env.block.coinbase.clone();
        let partition_db = PartitionDB::new(coinbase.clone(), scheduler_db);
        Self {
            spec_id,
            env,
            coinbase,
            partition_id,
            tx_states,
            txs,
            partition_db,
            assigned_txs,
            error_txs: HashMap::new(),
            tx_dependency: vec![],
            metrics: Default::default(),
        }
    }

    pub(crate) fn execute(&mut self) {
        let start = Instant::now();
        let mut has_unconfirmed_tx = false;
        let mut update_write_set: HashSet<LocationAndType> = HashSet::new();
        let mut evm = EvmBuilder::default()
            .with_db(&mut self.partition_db)
            .with_spec_id(self.spec_id)
            .with_env(Box::new(self.env.clone()))
            .build();

        #[allow(invalid_reference_casting)]
        let tx_states =
            unsafe { &mut *(&(*self.tx_states) as *const Vec<TxState> as *mut Vec<TxState>) };

        for txid in &self.assigned_txs {
            let txid = *txid;

            let mut miner_involved = false;
            if let Some(tx) = self.txs.get(txid) {
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
            let mut should_execute = true;
            if tx_states[txid].tx_status == TransactionStatus::Unconfirmed {
                has_unconfirmed_tx = true;
                // The unconfirmed transactions from the previous round may not require repeated execution.
                // The verification process is as follows:
                // 1. create a write set(update_write_set) to store write_set of previous conflict tx
                // 2. if an unconfirmed tx rerun, store write_set to update_write_set
                // 3. when running an unconfirmed tx, take it's pre-round read_set to join with update_write_set
                // 4. if the intersection is None, the unconfirmed tx no need to rerun, otherwise rerun
                if tx_states[txid].read_set.is_disjoint(&update_write_set) {
                    let transition = &tx_states[txid].execute_result.transition;
                    evm.db_mut().temporary_commit_transition(transition);
                    should_execute = false;
                    self.metrics.reusable_tx_cnt += 1;
                    tx_states[txid].tx_status = TransactionStatus::SkipValidation;
                }
            }
            if should_execute {
                evm.db_mut().miner_involved = miner_involved;
                let result = evm.transact();
                match result {
                    Ok(mut result_and_state) => {
                        let read_set = evm.db_mut().take_read_set();
                        let (write_set, rewards) =
                            evm.db().generate_write_set(&result_and_state.state);
                        if has_unconfirmed_tx {
                            update_write_set.extend(write_set.clone().into_iter());
                        }

                        let mut skip_validation = true;
                        if !read_set.is_subset(&tx_states[txid].read_set) {
                            skip_validation = false;
                        }
                        if skip_validation && !write_set.is_subset(&tx_states[txid].write_set) {
                            skip_validation = false;
                        }

                        let ResultAndState { result, mut state } = result_and_state;
                        if rewards.is_some() {
                            // remove miner's state if we handle rewards separately
                            state.remove(&self.coinbase);
                        }
                        // temporary commit to cache_db, to make use the remaining txs can read the updated data
                        let transition = evm.db_mut().temporary_commit(state);
                        tx_states[txid] = TxState {
                            tx_status: if skip_validation {
                                TransactionStatus::SkipValidation
                            } else {
                                TransactionStatus::Executed
                            },
                            read_set,
                            write_set,
                            execute_result: ResultAndTransition {
                                result: Some(result),
                                transition,
                                rewards: rewards.unwrap_or(0),
                            },
                        };
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

                        tx_states[txid] = TxState {
                            tx_status: TransactionStatus::Conflict,
                            read_set,
                            write_set,
                            execute_result: ResultAndTransition::default(),
                        };
                        self.error_txs.insert(txid, err);
                    }
                }
            }
        }
        self.metrics.execute_time = start.elapsed();
    }
}
