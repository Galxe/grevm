//! Block-order finalization of speculative transaction results.
//!
//! This stage validates each transaction nonce against the committed prefix, applies its state and
//! deferred beneficiary reward, then appends its outcome. The scheduler publishes the new committed
//! boundary only after all three writes complete.

use revm::{DatabaseCommit, DatabaseRef};
use revm_context::{
    Transaction, TxEnv,
    result::{EVMError, ExecutionResult, ResultAndState},
};
use revm_primitives::{Address, hardfork::SpecId};

use crate::{GrevmError, TxExecutionOutcome, TxId, parallel_state::ParallelStateCommit};
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
///
/// Deferring beneficiary rewards keeps the block-wide beneficiary write out of speculative
/// read/write sets. The committer still applies every transaction's state, reward, and outcome in
/// block order before the scheduler exposes the new committed prefix.
pub(crate) struct OrderedCommitter<'a, DB>
where
    DB: DatabaseRef,
{
    coinbase: Address,
    /// Active hardfork — needed to self-compute the coinbase reward (EIP-1559 basefee burn from
    /// LONDON onward).
    spec: SpecId,
    /// Block base fee per gas — the burned portion that does not reach the coinbase post-LONDON.
    basefee: u64,
    state: ParallelStateCommit<'a, DB>,
    disable_nonce_check: bool,
}

impl<'a, DB> OrderedCommitter<'a, DB>
where
    DB: DatabaseRef,
{
    /// Construct a committer after preloading the beneficiary account.
    ///
    /// Preloading surfaces a database failure before any ordered state mutation and ensures later
    /// reward credits use the cached account.
    pub(crate) fn try_new(
        coinbase: Address,
        spec: SpecId,
        basefee: u64,
        state: ParallelStateCommit<'a, DB>,
        disable_nonce_check: bool,
    ) -> Result<Self, DB::Error> {
        let committer = Self { coinbase, spec, basefee, state, disable_nonce_check };
        // Reward application must not discover a missing database value after transaction state
        // has already been committed.
        committer.state.basic_ref(coinbase)?;
        Ok(committer)
    }

    /// Compute the beneficiary reward for one transaction, mirroring revm's
    /// `post_execution::reward_beneficiary`: from LONDON the basefee is burned and only the
    /// remainder of the effective gas price reaches the beneficiary (EIP-1559).
    ///
    /// `result.gas_used()` is exactly the `gas.used()` (post-refund) that revm bills the reward on,
    /// so deferred commit reproduces revm's immediate reward calculation.
    fn compute_reward(&self, tx_env: &TxEnv, result: &ExecutionResult) -> u128 {
        let basefee = self.basefee as u128;
        let effective_gas_price = tx_env.effective_gas_price(basefee);
        let coinbase_gas_price = if self.spec.is_enabled_in(SpecId::LONDON) {
            effective_gas_price.saturating_sub(basefee)
        } else {
            effective_gas_price
        };
        coinbase_gas_price.saturating_mul(result.gas_used() as u128)
    }

    /// Commit one speculative result at the current ordered boundary.
    ///
    /// The caller must provide transactions contiguously in block order and publish the returned
    /// boundary only after this method succeeds. Nonce validation reads the already committed
    /// prefix; successful state, beneficiary reward, and outcome writes occur in that order.
    pub(crate) fn commit(
        &mut self,
        txid: TxId,
        tx_env: &TxEnv,
        result_and_state: ResultAndState,
        output: &mut OrderedCommitOutput,
    ) -> Result<CommitOutcome, GrevmError<DB::Error>> {
        // Workers retain the original transaction nonce but execute with revm's state nonce check
        // disabled. Recheck it here against the ordered, committed state.
        let ResultAndState { result, state } = result_and_state;
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
        // Deferred-beneficiary execution suppresses revm's immediate credit, so reproduce the
        // protocol reward at the ordered boundary.
        let reward = self.compute_reward(tx_env, &result);
        self.state.commit(state);

        // Deferral removes the ubiquitous beneficiary write conflict from speculation. Transactions
        // that need committed-origin data are separately gated by the scheduler's committed prefix.
        let coinbase = self.coinbase;
        self.state
            .increment_balances([(coinbase, reward)])
            .map_err(|error| GrevmError { txid, error: EVMError::Database(error) })?;
        Ok(CommitOutcome::Committed(output.push(result)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ParallelState;
    use revm_context::{
        DBErrorMarker,
        either::Either,
        result::{Output, SuccessReason},
        transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    };
    use revm_database::EmptyDB;
    use revm_primitives::{Address, B256, Bytes, HashMap, U256};
    use revm_state::{Account, AccountInfo, AccountStatus, Bytecode, EvmStorage};
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
        }
    }

    fn make_result_and_state(caller: Address, post_nonce: u64) -> ResultAndState {
        let mut state: HashMap<Address, Account> = HashMap::default();
        state.insert(
            caller,
            Account {
                info: make_account_info(post_nonce),
                transaction_id: 0,
                storage: EvmStorage::default(),
                status: AccountStatus::Touched,
            },
        );
        ResultAndState {
            result: ExecutionResult::Success {
                reason: SuccessReason::Stop,
                gas_used: 21_000,
                gas_refunded: 0,
                logs: Vec::new(),
                output: Output::Call(Bytes::new()),
            },
            state,
        }
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
            let mut commit = OrderedCommitter::try_new(
                Address::ZERO,
                SpecId::PRAGUE,
                0, // The zero gas price makes the reward zero regardless of base fee.
                commit_state,
                false,
            )
            .expect("beneficiary preload");
            let mut output = OrderedCommitOutput::default();
            assert!(matches!(
                commit
                    .commit(0, &tx_env, make_result_and_state(caller, post_nonce), &mut output,)
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

    /// `compute_reward` mirrors revm's `post_execution::reward_beneficiary`: pre-LONDON the full
    /// effective gas price reaches the beneficiary; from LONDON the basefee is burned (EIP-1559).
    #[test]
    fn compute_reward_matches_eip1559_basefee_burn() {
        let mut state = ParallelState::new(EmptyDB::default(), true, false);

        let gas_used = 21_000u64;
        let result = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas_used,
            gas_refunded: 0,
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        };
        // Legacy tx: effective_gas_price == gas_price regardless of basefee.
        let tx_env = TxEnv { gas_price: 100, gas_limit: gas_used, ..Default::default() };
        let basefee = 10u64;

        // Pre-LONDON: full price reaches the beneficiary; basefee is not subtracted.
        {
            let (_, commit_state) = state.split_for_parallel();
            let pre = OrderedCommitter::try_new(
                Address::ZERO,
                SpecId::BERLIN,
                basefee,
                commit_state,
                true,
            )
            .expect("coinbase preload");
            assert_eq!(pre.compute_reward(&tx_env, &result), 100u128 * gas_used as u128);
        }

        // LONDON+: basefee is burned; only the priority portion reaches the beneficiary.
        let (_, commit_state) = state.split_for_parallel();
        let post =
            OrderedCommitter::try_new(Address::ZERO, SpecId::LONDON, basefee, commit_state, true)
                .expect("coinbase preload");
        assert_eq!(post.compute_reward(&tx_env, &result), (100u128 - 10) * gas_used as u128);
    }

    #[test]
    fn coinbase_database_error_prevents_committer_construction() {
        let coinbase = Address::from([0xCB; 20]);
        let mut state = ParallelState::new(FailingDb, true, false);
        let (_, commit_state) = state.split_for_parallel();

        assert!(matches!(
            OrderedCommitter::try_new(coinbase, SpecId::PRAGUE, 0, commit_state, true),
            Err(TestDbError)
        ));
    }

    #[test]
    fn nonce_database_error_is_returned_with_txid() {
        let caller = Address::from([0xCC; 20]);
        let mut state = ParallelState::new(FailingDb, true, false);
        state.insert_account(Address::ZERO, make_account_info(0));
        let (_, commit_state) = state.split_for_parallel();
        let mut commit =
            OrderedCommitter::try_new(Address::ZERO, SpecId::PRAGUE, 0, commit_state, false)
                .expect("coinbase preload");
        let tx_env = TxEnv { caller, ..Default::default() };
        let mut output = OrderedCommitOutput::default();

        let error = commit
            .commit(11, &tx_env, make_result_and_state(caller, 1), &mut output)
            .expect_err("nonce lookup DB error must be returned");

        assert_eq!(error.txid, 11);
        assert!(matches!(error.error, EVMError::Database(TestDbError)));
    }

    #[test]
    fn nonce_mismatch_is_left_uncommitted_for_sequential_revalidation() {
        let caller = Address::from([0xCC; 20]);
        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        let (_, commit_state) = state.split_for_parallel();
        let mut commit =
            OrderedCommitter::try_new(Address::ZERO, SpecId::PRAGUE, 0, commit_state, false)
                .expect("coinbase preload");
        let tx_env = TxEnv { caller, nonce: 1, ..Default::default() };
        let mut output = OrderedCommitOutput::default();

        let outcome = commit
            .commit(0, &tx_env, make_result_and_state(caller, 1), &mut output)
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
        let mut commit =
            OrderedCommitter::try_new(Address::ZERO, SpecId::PRAGUE, 0, commit_state, false)
                .expect("coinbase preload");
        let tx_env = TxEnv { caller, nonce: u64::MAX, ..Default::default() };
        let mut output = OrderedCommitOutput::default();

        let outcome = commit
            .commit(9, &tx_env, make_result_and_state(caller, u64::MAX), &mut output)
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
            gas_used: 21_000,
            gas_refunded: 0,
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        });
        assert_eq!(committed, CommittedPrefixEnd::for_test(1));
        assert_eq!(output.into_outcomes().len(), 1);
    }
}
