//! Block-order finalization of speculative transaction results.
//!
//! This stage validates each transaction nonce against the committed prefix, folds a deferred
//! beneficiary reward into the transaction state, and commits that state once before publishing
//! the new boundary.

use revm::{DatabaseCommit, DatabaseRef};
use revm_context::{
    TxEnv,
    result::{EVMError, ExecutionResult},
};
use revm_primitives::Address;
use revm_state::Account;

use crate::{
    GrevmError, TxExecutionOutcome, TxId, beneficiary::SpeculativeResult,
    parallel_state::ParallelStateCommit,
};
use std::cmp::Ordering;

#[derive(Debug)]
pub(crate) enum CommitOutcome {
    /// The speculative result was committed successfully.
    Committed(CommittedPrefixEnd),
    /// The transaction must be revalidated sequentially from the committed prefix.
    NeedsSequentialFallback,
}

/// Exclusive end index of the state and outcomes committed in block order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CommittedPrefixEnd(TxId);

impl CommittedPrefixEnd {
    pub(crate) const ZERO: Self = Self(0);

    fn new(index: TxId) -> Self {
        Self(index)
    }

    pub(crate) fn index(self) -> TxId {
        self.0
    }

    #[cfg(test)]
    pub(super) fn for_test(index: TxId) -> Self {
        Self(index)
    }
}

/// Outcomes produced by the ordered commit stage.
#[derive(Debug, Default)]
pub(crate) struct OrderedCommitOutput {
    outcomes: Vec<TxExecutionOutcome>,
}

impl OrderedCommitOutput {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self { outcomes: Vec::with_capacity(capacity) }
    }

    pub(crate) fn end(&self) -> CommittedPrefixEnd {
        CommittedPrefixEnd::new(self.outcomes.len())
    }

    pub(crate) fn push(&mut self, result: ExecutionResult) -> CommittedPrefixEnd {
        self.outcomes.push(TxExecutionOutcome::Executed(result));
        self.end()
    }

    pub(crate) fn into_outcomes(self) -> Vec<TxExecutionOutcome> {
        self.outcomes
    }
}

/// Applies finalized transaction state in block order.
pub(crate) struct OrderedCommitter<'a, DB>
where
    DB: DatabaseRef,
{
    beneficiary: Address,
    state: ParallelStateCommit<'a, DB>,
    disable_nonce_check: bool,
}

impl<'a, DB> OrderedCommitter<'a, DB>
where
    DB: DatabaseRef,
{
    pub(crate) fn new(
        beneficiary: Address,
        state: ParallelStateCommit<'a, DB>,
        disable_nonce_check: bool,
    ) -> Self {
        Self { beneficiary, state, disable_nonce_check }
    }

    /// Commit one speculative result at the current ordered boundary.
    ///
    /// The caller must provide transactions contiguously in block order and publish the returned
    /// boundary only after this method succeeds. Nonce validation reads the already committed
    /// prefix. The scheduler preloads the beneficiary before workers start, so a deferred lookup
    /// reads the shared cache rather than discovering new database state during commit.
    pub(crate) fn commit(
        &mut self,
        txid: TxId,
        tx_env: &TxEnv,
        speculative_result: SpeculativeResult,
        output: &mut OrderedCommitOutput,
    ) -> Result<CommitOutcome, GrevmError<DB::Error>> {
        // Workers retain the original transaction nonce but execute with revm's state nonce check
        // disabled. Recheck it here against the ordered, committed state.
        let (result_and_state, deferred_reward) = speculative_result.into_commit_parts();
        let result = result_and_state.result;
        let mut state = result_and_state.state;
        if !self.disable_nonce_check {
            match self.state.basic_ref(tx_env.caller) {
                Ok(info) => {
                    // A non-existent account has Ethereum's default nonce of zero.
                    let expect = info.map_or(0, |info| info.nonce);
                    if tx_env.nonce == u64::MAX && expect == u64::MAX {
                        // Leave the speculative result uncommitted and let sequential execution
                        // classify the nonce overflow as an invalid transaction skip.
                        return Ok(CommitOutcome::NeedsSequentialFallback);
                    }
                    match tx_env.nonce.cmp(&expect) {
                        Ordering::Greater => {
                            // Do not finalize the speculative nonce verdict here. Leave this
                            // transaction out of `results` so sequential fallback starts at this
                            // exact transaction and validates it again against committed state.
                            return Ok(CommitOutcome::NeedsSequentialFallback);
                        }
                        Ordering::Less => {
                            // See the nonce-too-high branch above: fallback owns the final outcome.
                            return Ok(CommitOutcome::NeedsSequentialFallback);
                        }
                        _ => {}
                    }
                }
                Err(e) => {
                    return Err(GrevmError { txid, error: EVMError::Database(e) });
                }
            }
        }
        if let Some(reward) = deferred_reward {
            assert!(
                !state.contains_key(&self.beneficiary),
                "a deferred reward must not accompany a beneficiary state write",
            );
            let info = self
                .state
                .basic_ref(self.beneficiary)
                .map_err(|error| GrevmError { txid, error: EVMError::Database(error) })?;
            let mut account = Account::from(reward.apply_to(info));
            account.mark_touch();
            let _ = state.insert(self.beneficiary, account);
        }
        self.state.commit(state);
        Ok(CommitOutcome::Committed(output.push(result)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParallelState, beneficiary::DeferredBeneficiaryReward};
    use revm_context::{
        DBErrorMarker,
        either::Either,
        result::{Output, ResultAndState, ResultGas, SuccessReason},
        transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    };
    use revm_database::EmptyDB;
    use revm_primitives::{Address, B256, Bytes, U256};
    use revm_state::{Account, AccountInfo, AccountStatus, Bytecode};
    use std::fmt::{Display, Formatter};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestDbError;

    impl Display for TestDbError {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            f.write_str("test database error")
        }
    }

    impl core::error::Error for TestDbError {}
    impl DBErrorMarker for TestDbError {}

    #[derive(Clone, Debug, Default)]
    struct FailingDb;

    impl DatabaseRef for FailingDb {
        type Error = TestDbError;

        fn basic_ref(&self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(TestDbError)
        }

        fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Err(TestDbError)
        }

        fn storage_ref(&self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Err(TestDbError)
        }

        fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
            Err(TestDbError)
        }
    }

    fn make_account_info(nonce: u64) -> AccountInfo {
        AccountInfo {
            balance: U256::from(10u128.pow(18)),
            nonce,
            code_hash: B256::ZERO,
            code: None,
            ..Default::default()
        }
    }

    fn make_account(info: AccountInfo) -> Account {
        let mut account = Account::default();
        account.info = info;
        account.status = AccountStatus::Touched;
        account
    }

    fn make_result_and_state(caller: Address, post_nonce: u64) -> ResultAndState {
        let mut state = revm_primitives::AddressMap::default();
        state.insert(caller, make_account(make_account_info(post_nonce)));
        ResultAndState {
            result: ExecutionResult::Success {
                reason: SuccessReason::Stop,
                gas: ResultGas::default().with_total_gas_spent(21_000),
                logs: Vec::new(),
                output: Output::Call(Bytes::new()),
            },
            state,
        }
    }

    fn make_speculative_result(caller: Address, post_nonce: u64) -> SpeculativeResult {
        SpeculativeResult::settled(make_result_and_state(caller, post_nonce))
    }

    fn make_tx_env_with_auth(
        caller: Address,
        pre_nonce: u64,
        authorizations: Vec<(Address, u64)>,
    ) -> TxEnv {
        let authorization_list = authorizations
            .into_iter()
            .map(|(authority, nonce)| {
                let inner = Authorization {
                    chain_id: U256::ZERO,
                    address: Address::from([0xDE; 20]),
                    nonce,
                };
                Either::Right(RecoveredAuthorization::new_unchecked(
                    inner,
                    RecoveredAuthority::Valid(authority),
                ))
            })
            .collect();
        TxEnv { tx_type: 4, caller, nonce: pre_nonce, authorization_list, ..Default::default() }
    }

    fn run_commit(state: &mut ParallelState<EmptyDB>, tx_env: TxEnv, post_nonce: u64) {
        let caller = tx_env.caller;
        {
            let (_, commit_state) = state.split_for_parallel();
            let mut commit = OrderedCommitter::new(Address::ZERO, commit_state, false);
            let mut output = OrderedCommitOutput::default();
            assert!(matches!(
                commit
                    .commit(0, &tx_env, make_speculative_result(caller, post_nonce), &mut output,)
                    .expect("commit"),
                CommitOutcome::Committed(_)
            ));
            assert_eq!(output.end(), CommittedPrefixEnd::for_test(1));
        }
        assert_eq!(
            state.basic_ref(caller).expect("state read").expect("caller account").nonce,
            post_nonce
        );
    }

    fn commit_deferred_reward(
        state: &mut ParallelState<EmptyDB>,
        beneficiary: Address,
        reward: U256,
        txid: TxId,
    ) {
        let (_, commit_state) = state.split_for_parallel();
        let mut commit = OrderedCommitter::new(beneficiary, commit_state, true);
        let mut result_and_state = make_result_and_state(Address::ZERO, 0);
        result_and_state.state.clear();
        let speculative = SpeculativeResult::deferred(
            result_and_state,
            DeferredBeneficiaryReward::for_test(reward),
        );
        commit
            .commit(txid, &TxEnv::default(), speculative, &mut OrderedCommitOutput::default())
            .expect("deferred reward commit");
    }

    /// Model a result in which revm accepted one self-authorization after the outer nonce bump.
    /// Ordered commit validates the pre-state transaction nonce and preserves the supplied EVM
    /// post-state.
    #[test]
    fn commit_applies_single_eip7702_authority_nonce_bump() {
        let caller = Address::from([0xCA; 20]);
        let pre_nonce = 15u64;
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(pre_nonce));

        let tx_env = make_tx_env_with_auth(caller, pre_nonce, vec![(caller, pre_nonce + 1)]);
        run_commit(&mut state, tx_env, pre_nonce + 2);
    }

    /// Model two accepted self-authorizations with consecutive authority nonces.
    #[test]
    fn commit_applies_multiple_eip7702_authority_nonce_bumps() {
        let caller = Address::from([0xCB; 20]);
        let pre_nonce = 7u64;
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(pre_nonce));

        let tx_env = make_tx_env_with_auth(
            caller,
            pre_nonce,
            vec![(caller, pre_nonce + 1), (caller, pre_nonce + 2)],
        );
        run_commit(&mut state, tx_env, pre_nonce + 3);
    }

    /// Model revm skipping a self-authorization with a mismatched nonce. Ordered commit applies the
    /// supplied outer-transaction post-state without inferring another increment from the list.
    #[test]
    fn commit_applies_post_state_when_eip7702_authorization_is_skipped() {
        let caller = Address::from([0xCE; 20]);
        let pre_nonce = 42u64;
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(pre_nonce));

        let tx_env = make_tx_env_with_auth(caller, pre_nonce, vec![(caller, pre_nonce + 9)]);
        run_commit(&mut state, tx_env, pre_nonce + 1);
    }

    #[test]
    fn commit_applies_deferred_reward_without_double_crediting_immediate_state() {
        let beneficiary = Address::with_last_byte(0xBB);
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        commit_deferred_reward(&mut state, beneficiary, U256::from(7), 0);
        assert_eq!(state.basic_ref(beneficiary).unwrap().unwrap().balance, U256::from(7));

        {
            let (_, commit_state) = state.split_for_parallel();
            let mut commit = OrderedCommitter::new(beneficiary, commit_state, true);
            let mut result_and_state = make_result_and_state(Address::ZERO, 0);
            result_and_state.state.clear();
            result_and_state.state.insert(
                beneficiary,
                make_account(AccountInfo { balance: U256::from(18), ..Default::default() }),
            );
            let speculative = SpeculativeResult::settled(result_and_state);
            commit
                .commit(1, &TxEnv::default(), speculative, &mut OrderedCommitOutput::default())
                .unwrap();
        }
        assert_eq!(
            state.basic_ref(beneficiary).unwrap().unwrap().balance,
            U256::from(18),
            "an immediately applied reward must not be credited twice"
        );
    }

    #[test]
    #[should_panic(expected = "a deferred reward must not accompany a beneficiary state write")]
    fn commit_rejects_deferred_reward_with_beneficiary_state_write() {
        let beneficiary = Address::with_last_byte(0xBB);
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        let (_, commit_state) = state.split_for_parallel();
        let mut commit = OrderedCommitter::new(beneficiary, commit_state, true);
        let mut result_and_state = make_result_and_state(Address::ZERO, 0);
        result_and_state.state.insert(beneficiary, Account::default());
        let speculative = SpeculativeResult::deferred(
            result_and_state,
            DeferredBeneficiaryReward::for_test(U256::from(1)),
        );

        let _ =
            commit.commit(0, &TxEnv::default(), speculative, &mut OrderedCommitOutput::default());
    }

    #[test]
    fn deferred_reward_preserves_account_fields_and_checked_add_overflow() {
        let beneficiary = Address::with_last_byte(0xBC);
        let code_hash = B256::from([0x11; 32]);
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(
            beneficiary,
            AccountInfo {
                balance: U256::MAX - U256::from(1),
                nonce: 7,
                code_hash,
                ..Default::default()
            },
        );

        commit_deferred_reward(&mut state, beneficiary, U256::from(1), 0);
        commit_deferred_reward(&mut state, beneficiary, U256::from(1), 1);

        let info = state.basic_ref(beneficiary).unwrap().unwrap();
        assert_eq!(info.balance, U256::MAX);
        assert_eq!(info.nonce, 7);
        assert_eq!(info.code_hash, code_hash);
    }

    #[test]
    fn deferred_reward_database_error_is_returned_before_commit() {
        let beneficiary = Address::from([0xCB; 20]);
        let sentinel = Address::from([0xCC; 20]);
        let mut state = ParallelState::new(FailingDb, true, false);
        state.insert_account(sentinel, make_account_info(0));
        let error = {
            let (_, commit_state) = state.split_for_parallel();
            let mut commit = OrderedCommitter::new(beneficiary, commit_state, true);
            let result_and_state = make_result_and_state(sentinel, 1);
            let speculative = SpeculativeResult::deferred(
                result_and_state,
                DeferredBeneficiaryReward::for_test(U256::from(1)),
            );
            let mut output = OrderedCommitOutput::default();
            let error = commit
                .commit(7, &TxEnv::default(), speculative, &mut output)
                .expect_err("beneficiary lookup must fail before commit");
            assert_eq!(output.end(), CommittedPrefixEnd::ZERO);
            error
        };
        assert_eq!(error.txid, 7);
        assert!(matches!(error.error, EVMError::Database(TestDbError)));
        assert_eq!(state.basic_ref(sentinel).unwrap().unwrap().nonce, 0);
    }

    #[test]
    fn nonce_database_error_is_returned_with_txid() {
        let caller = Address::from([0xCC; 20]);
        let mut state = ParallelState::new(FailingDb, true, false);
        let (_, commit_state) = state.split_for_parallel();
        let mut commit = OrderedCommitter::new(Address::ZERO, commit_state, false);
        let tx_env = TxEnv { caller, ..Default::default() };
        let mut output = OrderedCommitOutput::default();

        let error = commit
            .commit(11, &tx_env, make_speculative_result(caller, 1), &mut output)
            .expect_err("nonce lookup DB error must be returned");

        assert_eq!(error.txid, 11);
        assert!(matches!(error.error, EVMError::Database(TestDbError)));
    }

    #[test]
    fn nonce_mismatch_is_left_uncommitted_for_sequential_revalidation() {
        let caller = Address::from([0xCC; 20]);
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        let (_, commit_state) = state.split_for_parallel();
        let mut commit = OrderedCommitter::new(Address::ZERO, commit_state, false);
        let tx_env = TxEnv { caller, nonce: 1, ..Default::default() };
        let mut output = OrderedCommitOutput::default();

        let outcome = commit
            .commit(0, &tx_env, make_speculative_result(caller, 1), &mut output)
            .expect("nonce lookup");

        assert!(matches!(outcome, CommitOutcome::NeedsSequentialFallback));
        assert_eq!(output.end(), CommittedPrefixEnd::ZERO);
    }

    #[test]
    fn max_nonce_requests_sequential_fallback() {
        let caller = Address::from([0xCD; 20]);
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(u64::MAX));
        let (_, commit_state) = state.split_for_parallel();
        let mut commit = OrderedCommitter::new(Address::ZERO, commit_state, false);
        let tx_env = TxEnv { caller, nonce: u64::MAX, ..Default::default() };
        let mut output = OrderedCommitOutput::default();

        let outcome = commit
            .commit(9, &tx_env, make_speculative_result(caller, u64::MAX), &mut output)
            .expect("nonce lookup");

        assert!(matches!(outcome, CommitOutcome::NeedsSequentialFallback));
        assert_eq!(output.end(), CommittedPrefixEnd::ZERO);
    }

    #[test]
    fn ordered_output_tracks_a_contiguous_committed_prefix() {
        let mut output = OrderedCommitOutput::default();
        assert_eq!(output.end(), CommittedPrefixEnd::ZERO);
        let committed = output.push(ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default().with_total_gas_spent(21_000),
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        });
        assert_eq!(committed, CommittedPrefixEnd::for_test(1));
        assert_eq!(output.into_outcomes().len(), 1);
    }
}
