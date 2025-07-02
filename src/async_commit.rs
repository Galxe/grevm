use revm::{Database, DatabaseCommit, DatabaseRef};
use revm_context::{
    TxEnv,
    result::{EVMError, ExecutionResult, InvalidTransaction, ResultAndState},
};
use revm_primitives::Address;

use crate::{GrevmError, ParallelState, TxId};
use std::cmp::Ordering;

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
    state: &'a ParallelState<DB>,
    commit_result: Result<(), GrevmError<DB::Error>>,
    disable_nonce_check: bool,
}

impl<'a, DB> StateAsyncCommit<'a, DB>
where
    DB: DatabaseRef,
{
    pub(crate) fn new(
        coinbase: Address,
        state: &'a ParallelState<DB>,
        disable_nonce_check: bool,
    ) -> Self {
        Self { coinbase, results: vec![], state, commit_result: Ok(()), disable_nonce_check }
    }

    fn state_mut(&self) -> &mut ParallelState<DB> {
        #[allow(invalid_reference_casting)]
        unsafe {
            &mut *(self.state as *const ParallelState<DB> as *mut ParallelState<DB>)
        }
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
        if !self.disable_nonce_check {
            match self.state.basic_ref(tx_env.caller) {
                Ok(info) => {
                    if let Some(info) = info {
                        let state = info.nonce;
                        match tx_env.nonce.cmp(&state) {
                            Ordering::Greater => {
                                self.commit_result = Err(GrevmError {
                                    txid,
                                    error: EVMError::Transaction(
                                        InvalidTransaction::NonceTooHigh {
                                            tx: tx_env.nonce,
                                            state,
                                        },
                                    ),
                                });
                            }
                            Ordering::Less => {
                                self.commit_result = Err(GrevmError {
                                    txid,
                                    error: EVMError::Transaction(InvalidTransaction::NonceTooLow {
                                        tx: tx_env.nonce,
                                        state,
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
        let ResultAndState { result, state, lazy_reward } = result_and_state;
        self.results.push(result);
        self.state_mut().commit(state);
        // In Ethereum, each transaction includes a miner reward, which would introduce write
        // conflicts in the read-write set if implemented naively, preventing parallel transaction
        // execution. Grevm adopts an optimized approach: it defers miner reward distribution until
        // the transaction commitment phase rather than during execution. This design ensures
        // correct concurrency - even if subsequent transactions access the miner's account, they
        // will read the proper miner state from ParallelState (verified via commit_idx) without
        // creating artificial dependencies.
        assert!(self.state_mut().increment_balances(vec![(self.coinbase, lazy_reward)]).is_ok());
    }
}
