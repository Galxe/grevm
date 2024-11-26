use crate::{
    scheduler::RewardsAccumulators,
    storage::{PartitionDB, SchedulerDB},
    LocationAndType, PartitionId, ResultAndTransition, SharedTxStates, TransactionStatus, TxId,
    TxState,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use revm::{
    primitives::{Address, EVMError, Env, ResultAndState, SpecId, TxEnv, TxKind},
    DatabaseRef, EvmBuilder,
};
use revm_primitives::db::Database;
use std::{
    collections::BTreeSet,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct PartitionMetrics {
    pub execute_time: Duration,
    pub reusable_tx_cnt: u64,
}

/// The `PartitionExecutor` is tasked with executing transactions within a specific partition.
/// Transactions are executed optimistically, and their read and write sets are recorded after
/// execution. If a transaction fails, it is marked as a conflict; if it succeeds, it is marked as
/// unconfirmed. The final state of a transaction is determined by the scheduler's validation
/// process, which leverages the recorded read/write sets and execution results.
/// This validation process uses the STM (Software Transactional Memory) algorithm, a standard
/// conflict-checking method. Additionally, the read/write sets are used to update transaction
/// dependencies. For instance, if transaction A's write set intersects with transaction B's read
/// set, then B depends on A. These dependencies are then used to construct the next round of
/// transaction partitions. The global state of transactions is maintained in `SharedTxStates`.
/// Since the state of a transaction is not modified by multiple threads simultaneously,
/// `SharedTxStates` is thread-safe. Unsafe code is used to convert `SharedTxStates` to
/// a mutable reference, allowing modification of transaction states during execution.
pub(crate) struct PartitionExecutor<DB>
where
    DB: DatabaseRef,
{
    spec_id: SpecId,
    env: Env,

    #[allow(dead_code)]
    coinbase: Address,
    #[allow(dead_code)]
    partition_id: PartitionId,

    /// SharedTxStates is a thread-safe global state of transactions.
    /// Unsafe code is used to convert SharedTxStates to a mutable reference,
    /// allowing modification of transaction states during execution
    tx_states: SharedTxStates,
    txs: Arc<Vec<TxEnv>>,

    rewards_accumulators: Arc<RewardsAccumulators>,

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
        rewards_accumulators: Arc<RewardsAccumulators>,
        scheduler_db: Arc<SchedulerDB<DB>>,
        txs: Arc<Vec<TxEnv>>,
        tx_states: SharedTxStates,
        assigned_txs: Vec<TxId>,
    ) -> Self {
        let coinbase = env.block.coinbase;
        let partition_db = PartitionDB::new(coinbase, scheduler_db, rewards_accumulators.clone());
        Self {
            spec_id,
            env,
            coinbase,
            partition_id,
            tx_states,
            txs,
            rewards_accumulators,
            partition_db,
            assigned_txs,
            error_txs: HashMap::new(),
            tx_dependency: vec![],
            metrics: Default::default(),
        }
    }

    /// Execute transactions in the partition
    /// The transactions are executed optimistically, and their read and write sets are recorded
    /// after execution. The final state of a transaction is determined by the scheduler's
    /// validation process.
    pub(crate) fn execute(&mut self) {
        let start = Instant::now();
        let coinbase = self.env.block.coinbase;
        let mut evm = EvmBuilder::default()
            .with_db(&mut self.partition_db)
            .with_spec_id(self.spec_id)
            .with_env(Box::new(std::mem::take(&mut self.env)))
            .build();

        #[allow(invalid_reference_casting)]
        let tx_states =
            unsafe { &mut *(&(*self.tx_states) as *const Vec<TxState> as *mut Vec<TxState>) };

        for txid in &self.assigned_txs {
            let txid = *txid;

            if let Some(tx) = self.txs.get(txid) {
                *evm.tx_mut() = tx.clone();
                evm.db_mut().current_txid = txid;
                evm.db_mut().raw_transfer = true; // no need to wait miner rewards
                let mut raw_transfer = true;
                if let Ok(Some(info)) = evm.db_mut().basic(tx.caller) {
                    raw_transfer = info.is_empty_code_hash();
                }
                if let TxKind::Call(to) = tx.transact_to {
                    if let Ok(Some(info)) = evm.db_mut().basic(to) {
                        raw_transfer &= info.is_empty_code_hash();
                    }
                }
                evm.db_mut().raw_transfer = raw_transfer;
                evm.db_mut().take_read_set(); // clean read set
            } else {
                panic!("Wrong transactions ID");
            }
            // If the transaction is unconfirmed, it may not require repeated execution
            let mut should_execute = true;
            let mut update_rewards = 0;
            if tx_states[txid].tx_status == TransactionStatus::Unconfirmed &&
                !self.rewards_accumulators.contains_key(&txid)
            {
                if evm.db_mut().check_read_set(&tx_states[txid].read_set) {
                    // Unconfirmed transactions from the previous round might not need to be
                    // re-executed.
                    let transition = &tx_states[txid].execute_result.transition;
                    evm.db_mut().temporary_commit_transition(transition);
                    should_execute = false;
                    self.metrics.reusable_tx_cnt += 1;
                    update_rewards = tx_states[txid].execute_result.rewards;
                    tx_states[txid].tx_status = TransactionStatus::SkipValidation;
                }
            }
            if should_execute {
                let result = evm.transact_lazy_reward();
                match result {
                    Ok(result_and_state) => {
                        let ResultAndState { result, mut state, rewards } = result_and_state;
                        let read_set = evm.db_mut().take_read_set();
                        let write_set = evm.db().generate_write_set(&mut state);

                        // Check if the transaction can be skipped
                        // skip_validation=true does not necessarily mean the transaction can skip
                        // validation. Only transactions with consecutive
                        // minimum TxID can skip validation. This is because
                        // if a transaction with a smaller TxID conflicts,
                        // the states of subsequent transactions are invalid.
                        let mut skip_validation =
                            !matches!(read_set.get(&LocationAndType::Basic(coinbase)), Some(None));
                        skip_validation &= !self.rewards_accumulators.contains_key(&txid);
                        skip_validation &=
                            read_set.iter().all(|l| tx_states[txid].read_set.contains_key(l.0));
                        skip_validation &=
                            write_set.iter().all(|l| tx_states[txid].write_set.contains(l));

                        // temporary commit to cache_db, to make use the remaining txs can read the
                        // updated data
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
                                rewards,
                            },
                        };
                        update_rewards = rewards;
                    }
                    Err(err) => {
                        // In a parallel execution environment, transactions might fail due to
                        // reasons such as dependent transfers not being
                        // received yet. Therefore, a transaction failure
                        // does not necessarily lead to a block failure.
                        // Only transactions that are in the FINALITY state can cause a block
                        // failure. During verification, failed transactions
                        // are marked as being in a conflict state.

                        // update read set
                        let mut read_set = evm.db_mut().take_read_set();
                        // update write set with the caller and transact_to
                        let mut write_set = HashSet::new();
                        read_set.entry(LocationAndType::Basic(evm.tx().caller)).or_insert(None);
                        write_set.insert(LocationAndType::Basic(evm.tx().caller));
                        if let TxKind::Call(to) = evm.tx().transact_to {
                            read_set.entry(LocationAndType::Basic(to)).or_insert(None);
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
            if !self.rewards_accumulators.is_empty() {
                Self::report_rewards(self.rewards_accumulators.clone(), txid, update_rewards);
            }
        }
        self.metrics.execute_time = start.elapsed();
    }

    fn report_rewards(rewards_accumulators: Arc<RewardsAccumulators>, txid: TxId, rewards: u128) {
        if !rewards_accumulators.is_empty() {
            for (_, accumulator) in rewards_accumulators.range((txid + 1)..) {
                let counter = accumulator.accumulate_counter.fetch_add(1, Ordering::Release);
                accumulator.accumulate_rewards.fetch_add(rewards, Ordering::Release);
                if counter >= accumulator.accumulate_num {
                    panic!("to many reward records!");
                }
                if counter == accumulator.accumulate_num - 1 {
                    accumulator.notifier.notify_one();
                }
            }
        }
    }
}
