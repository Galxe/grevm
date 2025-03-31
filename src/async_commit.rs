use crate::{storage::ParallelBundleState, GrevmError, ParallelState, TxId};
use revm::{
    db::{states::bundle_state::BundleRetention, BundleState},
    TransitionState,
};
use revm_primitives::{
    db::{Database, DatabaseCommit, DatabaseRef},
    Address, EVMError, ExecutionResult, InvalidTransaction, ResultAndState, TxEnv,
};
use std::cmp::Ordering;

pub struct StateAsyncCommit<'a, DB>
where
    DB: DatabaseRef,
{
    coinbase: Address,
    results: Vec<ExecutionResult>,
    state: &'a ParallelState<DB>,
    commit_result: Result<(), GrevmError<DB::Error>>,
}

impl<'a, DB> StateAsyncCommit<'a, DB>
where
    DB: DatabaseRef,
{
    pub fn new(coinbase: Address, state: &'a ParallelState<DB>) -> Self {
        Self { coinbase, results: vec![], state, commit_result: Ok(()) }
    }

    fn state_mut(&self) -> &mut ParallelState<DB> {
        #[allow(invalid_reference_casting)]
        unsafe {
            &mut *(self.state as *const ParallelState<DB> as *mut ParallelState<DB>)
        }
    }

    pub fn init(&mut self) -> Result<(), DB::Error> {
        match self.state_mut().basic(self.coinbase) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn take_result(&mut self) -> Vec<ExecutionResult> {
        std::mem::take(&mut self.results)
    }

    pub fn take_bundle(&mut self) -> BundleState {
        if let Some(transition_state) =
            self.state_mut().transition_state.as_mut().map(TransitionState::take)
        {
            self.state_mut().bundle_state.parallel_apply_transitions_and_create_reverts(
                transition_state,
                BundleRetention::Reverts,
            );
        }

        self.state_mut().take_bundle()
    }

    pub fn commit_result(&self) -> &Result<(), GrevmError<DB::Error>> {
        &self.commit_result
    }

    pub fn commit(&mut self, txid: TxId, tx_env: &TxEnv, result_and_state: ResultAndState) {
        // check nonce
        if let Some(tx) = tx_env.nonce {
            match self.state.basic_ref(tx_env.caller) {
                Ok(info) => {
                    if let Some(info) = info {
                        let state = info.nonce;
                        match tx.cmp(&state) {
                            Ordering::Greater => {
                                self.commit_result = Err(GrevmError {
                                    txid,
                                    error: EVMError::Transaction(
                                        InvalidTransaction::NonceTooHigh { tx, state },
                                    ),
                                });
                            }
                            Ordering::Less => {
                                self.commit_result = Err(GrevmError {
                                    txid,
                                    error: EVMError::Transaction(InvalidTransaction::NonceTooLow {
                                        tx,
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
        let ResultAndState { result, state, rewards } = result_and_state;
        self.results.push(result);
        self.state_mut().commit(state);
        assert!(self.state_mut().increment_balances(vec![(self.coinbase, rewards)]).is_ok());
    }
}
