use revm::{Database, DatabaseCommit, DatabaseRef};
use revm_context::{
    TxEnv,
    result::{EVMError, ExecutionResult, InvalidTransaction, ResultAndState},
};
use revm_primitives::Address;

use crate::{GrevmError, ParallelState, TxId};
use std::{cell::UnsafeCell, cmp::Ordering};

/// `StateAsyncCommit` asynchronously finalizes transaction states,
/// serving two critical purposes:
/// ensuring Ethereum-compatible execution results and resolving edge cases like miner rewards and
/// self-destructed accounts. Though state commits strictly follow transaction confirmation order
/// for correctness, the asynchronous pipeline eliminates any additional block execution latency by
/// decoupling finalization from the critical path.
pub(crate) struct StateAsyncCommit<'a, DB>
where
    DB: DatabaseRef,
{
    coinbase: Address,
    results: Vec<ExecutionResult>,
    /// SAFETY: The `UnsafeCell` allows the commit thread to mutate `ParallelState` while worker
    /// threads hold shared references for reads. This is safe because:
    /// 1. The commit thread is the sole writer (serialized by the finality ordering).
    /// 2. Worker threads only read via `DatabaseRef` methods which access `DashMap`-based caches
    ///    (inherently thread-safe) or the immutable underlying database.
    /// 3. `commit()` and `increment_balances()` only mutate `transition_state` (append-only
    ///    transitions) and `cache` (DashMap, thread-safe).
    state: &'a UnsafeCell<ParallelState<DB>>,
    commit_result: Result<(), GrevmError<DB::Error>>,
    disable_nonce_check: bool,
}

// SAFETY: StateAsyncCommit is only used within a single commit thread (never shared).
// The UnsafeCell<ParallelState<DB>> requires manual Send/Sync because UnsafeCell is !Sync,
// but our usage pattern guarantees single-writer access from the commit thread.
unsafe impl<DB: DatabaseRef + Send + Sync> Send for StateAsyncCommit<'_, DB>
where
    DB::Error: Send,
{
}

impl<'a, DB> StateAsyncCommit<'a, DB>
where
    DB: DatabaseRef,
{
    pub(crate) fn new(
        coinbase: Address,
        state: &'a UnsafeCell<ParallelState<DB>>,
        disable_nonce_check: bool,
    ) -> Self {
        Self { coinbase, results: vec![], state, commit_result: Ok(()), disable_nonce_check }
    }

    /// SAFETY: Caller must ensure exclusive mutable access — only the commit thread calls this,
    /// and it is serialized by the finality ordering protocol.
    fn state_mut(&self) -> &mut ParallelState<DB> {
        unsafe { &mut *self.state.get() }
    }

    /// SAFETY: Shared read access to state is safe because `ParallelState` reads go through
    /// `DashMap` (thread-safe) or the immutable database.
    fn state_ref(&self) -> &ParallelState<DB> {
        unsafe { &*self.state.get() }
    }

    pub(crate) fn init(&mut self) -> Result<(), DB::Error> {
        // Accesses the coinbase account to ensure proper handling of miner rewards (via
        // increment_balances) within ParallelState. This preemptive access guarantees correct state
        // synchronization when applying miner rewards during the final commitment phase.
        match self.state_mut().basic(self.coinbase) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub(crate) fn take_result(&mut self) -> Vec<ExecutionResult> {
        std::mem::take(&mut self.results)
    }

    pub(crate) fn commit_result(&self) -> &Result<(), GrevmError<DB::Error>> {
        &self.commit_result
    }

    pub(crate) fn commit(&mut self, txid: TxId, tx_env: &TxEnv, result_and_state: ResultAndState) {
        // During Grevm's execution, transaction nonces are temporarily set to `None` to bypass the
        // EVM's strict sequential nonce verification. This design enables concurrent transaction
        // processing without immediate validation failures. However, during the final commitment
        // phase, the system enforces strict nonce monotonicity checks to guarantee transaction
        // integrity and prevent double-spending attacks.
        let ResultAndState { result, state, lazy_reward } = result_and_state;
        if !self.disable_nonce_check {
            match self.state_ref().basic_ref(tx_env.caller) {
                Ok(info) => {
                    if let Some(info) = info {
                        let expect = info.nonce;
                        if let Some(change) = state.get(&tx_env.caller) {
                            if change.info.nonce != expect + 1 {
                                self.commit_result = Err(GrevmError {
                                    txid,
                                    error: EVMError::Transaction(
                                        InvalidTransaction::NonceTooHigh {
                                            tx: change.info.nonce,
                                            state: expect,
                                        },
                                    ),
                                });
                                return;
                            }
                        }
                        match tx_env.nonce.cmp(&expect) {
                            Ordering::Greater => {
                                self.commit_result = Err(GrevmError {
                                    txid,
                                    error: EVMError::Transaction(
                                        InvalidTransaction::NonceTooHigh {
                                            tx: tx_env.nonce,
                                            state: expect,
                                        },
                                    ),
                                });
                            }
                            Ordering::Less => {
                                self.commit_result = Err(GrevmError {
                                    txid,
                                    error: EVMError::Transaction(InvalidTransaction::NonceTooLow {
                                        tx: tx_env.nonce,
                                        state: expect,
                                    }),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    self.commit_result = Err(GrevmError { txid, error: EVMError::Database(e) })
                }
            }
        }
        self.results.push(result);
        self.state_mut().commit(state);

        // In Ethereum, each transaction includes a miner reward, which would introduce write
        // conflicts in the read-write set if implemented naively, preventing parallel transaction
        // execution. Grevm adopts an optimized approach: it defers miner reward distribution until
        // the transaction commitment phase rather than during execution. This design ensures
        // correct concurrency - even if subsequent transactions access the miner's account, they
        // will read the proper miner state from ParallelState (verified via commit_idx) without
        // creating artificial dependencies.
        if let Err(e) = self.state_mut().increment_balances(vec![(self.coinbase, lazy_reward)]) {
            self.commit_result = Err(GrevmError { txid, error: EVMError::Database(e) });
        }
    }
}
