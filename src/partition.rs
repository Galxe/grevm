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
pub(crate) struct PartitionMetrics {
    pub execute_time: Duration,
    pub reusable_tx_cnt: u64,
}

/// The `PartitionExecutor` is tasked with executing transactions within a specific partition.
/// Transactions are executed optimistically, and their read and write sets are recorded after execution.
/// If a transaction fails, it is marked as a conflict; if it succeeds, it is marked as unconfirmed.
/// The final state of a transaction is determined by the scheduler's validation process,
/// which leverages the recorded read/write sets and execution results.
/// This validation process uses the STM (Software Transactional Memory) algorithm, a standard conflict-checking method.
/// Additionally, the read/write sets are used to update transaction dependencies.
/// For instance, if transaction A's write set intersects with transaction B's read set, then B depends on A.
/// These dependencies are then used to construct the next round of transaction partitions.
/// The global state of transactions is maintained in `SharedTxStates`.
/// Since the state of a transaction is not modified by multiple threads simultaneously,
/// `SharedTxStates` is thread-safe. Unsafe code is used to convert `SharedTxStates` to
/// a mutable reference, allowing modification of transaction states during execution.
pub(crate) struct PartitionExecutor<DB>
where
    DB: DatabaseRef,
{
    spec_id: SpecId,
    env: Env,
    coinbase: Address,

    #[allow(dead_code)]
    partition_id: PartitionId,

    /// SharedTxStates is a thread-safe global state of transactions.
    /// Unsafe code is used to convert SharedTxStates to a mutable reference,
    /// allowing modification of transaction states during execution
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
        let coinbase = env.block.coinbase;
        let partition_db = PartitionDB::new(coinbase, scheduler_db);
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

    /// Check if there are unconfirmed transactions in the current partition
    /// If there are unconfirmed transactions,
    /// the update_write_set will be used to determine whether the transaction needs to be rerun
    fn has_unconfirmed_tx(tx_states: &Vec<TxState>, assigned_txs: &Vec<TxId>) -> bool {
        // If the first transaction is not executed, it means that the partition has not been executed
        if tx_states[0].tx_status == TransactionStatus::Initial {
            return false;
        }
        for txid in assigned_txs {
            if tx_states[*txid].tx_status == TransactionStatus::Unconfirmed {
                return true;
            }
        }
        false
    }

    /// Execute transactions in the partition
    /// The transactions are executed optimistically, and their read and write sets are recorded after execution.
    /// The final state of a transaction is determined by the scheduler's validation process.
    pub(crate) fn execute(&mut self) {
        let start = Instant::now();
        // the update_write_set is used to determine whether the transaction needs to be rerun
        let mut update_write_set: HashSet<LocationAndType> = HashSet::new();
        let mut evm = EvmBuilder::default()
            .with_db(&mut self.partition_db)
            .with_spec_id(self.spec_id)
            .with_env(Box::new(self.env.clone()))
            .build();

        #[allow(invalid_reference_casting)]
        let tx_states =
            unsafe { &mut *(&(*self.tx_states) as *const Vec<TxState> as *mut Vec<TxState>) };

        let has_unconfirmed_tx = Self::has_unconfirmed_tx(tx_states, &self.assigned_txs);
        for txid in &self.assigned_txs {
            let txid = *txid;

            // Miner is handled separately for each transaction
            // However, if the miner is involved in the transaction
            // we have to fully track the updates of this transaction to make sure the miner's account is updated correctly
            let mut miner_involved = false;
            if let Some(tx) = self.txs.get(txid) {
                *evm.tx_mut() = tx.clone();
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
            // If the transaction is unconfirmed, it may not require repeated execution
            let mut should_execute = true;
            if tx_states[txid].tx_status == TransactionStatus::Unconfirmed {
                // Unconfirmed transactions from the previous round might not need to be re-executed.
                // The verification process is as follows:
                // 1. Create an update_write_set to store the write sets of previous conflicting transactions.
                // 2. If an unconfirmed transaction is re-executed, add its write set to update_write_set.
                // 3. When processing an unconfirmed transaction, join its previous round's read set with update_write_set.
                // 4. If there is no intersection, the unconfirmed transaction does not need to be re-executed; otherwise, it does.
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
                    Ok(result_and_state) => {
                        let read_set = evm.db_mut().take_read_set();
                        let (write_set, rewards) =
                            evm.db().generate_write_set(&result_and_state.state);
                        if has_unconfirmed_tx {
                            update_write_set.extend(write_set.clone().into_iter());
                        }

                        // Check if the transaction can be skipped
                        // skip_validation=true does not necessarily mean the transaction can skip validation.
                        // Only transactions with consecutive minimum TxID can skip validation.
                        // This is because if a transaction with a smaller TxID conflicts,
                        // the states of subsequent transactions are invalid.
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
                        // In a parallel execution environment, transactions might fail due to reasons
                        // such as dependent transfers not being received yet.
                        // Therefore, a transaction failure does not necessarily lead to a block failure.
                        // Only transactions that are in the FINALITY state can cause a block failure.
                        // During verification, failed transactions are marked as being in a conflict state.

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
