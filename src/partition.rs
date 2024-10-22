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

    /// Execute transactions in the partition
    /// The transactions are executed optimistically, and their read and write sets are recorded after execution.
    /// The final state of a transaction is determined by the scheduler's validation process.
    pub(crate) fn execute(&mut self) {
        let start = Instant::now();
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

            if let Some(tx) = self.txs.get(txid) {
                *evm.tx_mut() = tx.clone();
            } else {
                panic!("Wrong transactions ID");
            }
            // If the transaction is unconfirmed, it may not require repeated execution
            let mut should_execute = true;
            if tx_states[txid].tx_status == TransactionStatus::Unconfirmed {
                if evm.db_mut().check_read_set(&tx_states[txid].read_set) {
                    // Unconfirmed transactions from the previous round might not need to be re-executed.
                    let transition = &tx_states[txid].execute_result.transition;
                    evm.db_mut().temporary_commit_transition(transition);
                    should_execute = false;
                    self.metrics.reusable_tx_cnt += 1;
                    tx_states[txid].tx_status = TransactionStatus::SkipValidation;
                }
            }
            if should_execute {
                let result = evm.transact();
                match result {
                    Ok(result_and_state) => {
                        let read_set = evm.db_mut().take_read_set();
                        let (write_set, miner_update) =
                            evm.db().generate_write_set(&result_and_state.state);

                        // Check if the transaction can be skipped
                        // skip_validation=true does not necessarily mean the transaction can skip validation.
                        // Only transactions with consecutive minimum TxID can skip validation.
                        // This is because if a transaction with a smaller TxID conflicts,
                        // the states of subsequent transactions are invalid.
                        let mut skip_validation =
                            read_set.iter().all(|l| tx_states[txid].read_set.contains_key(l.0));
                        skip_validation &=
                            write_set.iter().all(|l| tx_states[txid].write_set.contains(l));

                        let ResultAndState { result, mut state } = result_and_state;
                        if miner_update.is_some() {
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
                                miner_update: miner_update.unwrap_or_default(),
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
                        read_set.insert(LocationAndType::Basic(evm.tx().caller), None);
                        write_set.insert(LocationAndType::Basic(evm.tx().caller));
                        if let TxKind::Call(to) = evm.tx().transact_to {
                            read_set.insert(LocationAndType::Basic(to), None);
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
