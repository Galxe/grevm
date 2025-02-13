use crate::{storage::ParallelBundleState, ParallelState};
use revm::{
    db::{states::bundle_state::BundleRetention, BundleState},
    TransitionState,
};
use revm_primitives::{
    db::{DatabaseCommit, DatabaseRef},
    Address, ExecutionResult, ResultAndState,
};

pub struct StateAsyncCommit<'a, DB>
where
    DB: DatabaseRef,
{
    coinbase: Address,
    results: Vec<ExecutionResult>,
    state: &'a ParallelState<DB>,
}

impl<'a, DB> StateAsyncCommit<'a, DB>
where
    DB: DatabaseRef,
{
    pub fn new(coinbase: Address, state: &'a ParallelState<DB>) -> Self {
        Self { coinbase, results: vec![], state }
    }

    fn state_mut(&self) -> &mut ParallelState<DB> {
        #[allow(invalid_reference_casting)]
        unsafe {
            &mut *(self.state as *const ParallelState<DB> as *mut ParallelState<DB>)
        }
    }

    pub fn init(&mut self) -> Result<(), DB::Error> {
        match self.state_mut().load_cache_account(self.coinbase) {
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

    pub fn commit(&mut self, result_and_state: ResultAndState) {
        let ResultAndState { result, state, rewards } = result_and_state;
        self.results.push(result);
        self.state_mut().commit(state);
        assert!(self.state_mut().increment_balances(vec![(self.coinbase, rewards)]).is_ok());
    }
}
