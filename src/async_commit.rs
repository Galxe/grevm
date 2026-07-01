use revm::{Database, DatabaseCommit, DatabaseRef};
use revm_context::{
    Transaction, TxEnv,
    result::{EVMError, ExecutionResult, InvalidTransaction, ResultAndState},
};
use revm_primitives::{Address, hardfork::SpecId};

use crate::{GrevmError, ParallelState, TxId};
use std::{cell::UnsafeCell, cmp::Ordering};

/// Only constructible within the commit thread's scope (or sequential fallback).
/// Guarantees exclusive mutable access to ParallelState.
pub(crate) struct CommitGuard<'a, DB: DatabaseRef> {
    state: &'a UnsafeCell<ParallelState<DB>>,
}

impl<'a, DB: DatabaseRef> CommitGuard<'a, DB> {
    pub(crate) fn new(state: &'a UnsafeCell<ParallelState<DB>>) -> Self {
        Self { state }
    }

    pub(crate) fn state_mut(&mut self) -> &mut ParallelState<DB> {
        // SAFETY: CommitGuard requires `&mut self`, ensuring exclusive access
        // to the underlying state for the guard's owner.
        unsafe { &mut *self.state.get() }
    }
}

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
    /// Active hardfork — needed to self-compute the coinbase reward (EIP-1559 basefee burn from
    /// LONDON onward).
    spec: SpecId,
    /// Block base fee per gas — the burned portion that does not reach the coinbase post-LONDON.
    basefee: u64,
    results: Vec<ExecutionResult>,
    guard: CommitGuard<'a, DB>,
    commit_result: Result<(), GrevmError<DB::Error>>,
    disable_nonce_check: bool,
}

// SAFETY: StateAsyncCommit is only used within a single commit thread (never shared).
// CommitGuard ensures exclusive mutable access through its `&mut self` requirement.
unsafe impl<DB: DatabaseRef + Send + Sync> Send for StateAsyncCommit<'_, DB> where DB::Error: Send {}

impl<'a, DB> StateAsyncCommit<'a, DB>
where
    DB: DatabaseRef,
{
    pub(crate) fn new(
        coinbase: Address,
        spec: SpecId,
        basefee: u64,
        guard: CommitGuard<'a, DB>,
        disable_nonce_check: bool,
    ) -> Self {
        Self {
            coinbase,
            spec,
            basefee,
            results: vec![],
            guard,
            commit_result: Ok(()),
            disable_nonce_check,
        }
    }

    /// Self-compute the miner reward for one transaction, mirroring revm's
    /// `post_execution::reward_beneficiary`: from LONDON the basefee is burned and only the
    /// remainder of the effective gas price reaches the coinbase (EIP-1559).
    ///
    /// `result.gas_used()` is exactly the `gas.used()` (post-refund) that revm bills the reward on,
    /// so this reproduces the forked revm's `lazy_reward` without relying on it.
    fn compute_reward(&self, tx_env: &TxEnv, result: &ExecutionResult) -> u128 {
        let basefee = self.basefee as u128;
        let effective_gas_price = tx_env.effective_gas_price(basefee);
        let coinbase_gas_price = if self.spec.is_enabled_in(SpecId::LONDON) {
            effective_gas_price.saturating_sub(basefee)
        } else {
            effective_gas_price
        };
        coinbase_gas_price.saturating_mul(result.tx_gas_used() as u128)
    }

    pub(crate) fn state_mut(&mut self) -> &mut ParallelState<DB> {
        self.guard.state_mut()
    }

    /// SAFETY: Shared read access to state is safe because `ParallelState` reads go through
    /// `DashMap` (thread-safe) or the immutable database.
    fn state_ref(&self) -> &ParallelState<DB> {
        // Safe to get a shared reference since we only mutate exclusively when we have `&mut self`
        unsafe { &*self.guard.state.get() }
    }

    pub(crate) fn init(&mut self) -> Result<(), DB::Error> {
        // Accesses the coinbase account to ensure proper handling of miner rewards (via
        // increment_balances) within ParallelState. This preemptive access guarantees correct state
        // synchronization when applying miner rewards during the final commitment phase.
        let coinbase = self.coinbase;
        match self.state_mut().basic(coinbase) {
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
        let ResultAndState { result, state } = result_and_state;
        // Self-compute the miner reward: upstream revm credits the coinbase inside execution, which
        // grevm suppresses with a custom `Handler` (see `scheduler::NoRewardHandler`) and applies
        // here instead. This reproduces what the dropped `Galxe/revm` fork returned via
        // `ResultAndState.lazy_reward`.
        let reward = self.compute_reward(tx_env, &result);
        if !self.disable_nonce_check {
            match self.state_ref().basic_ref(tx_env.caller) {
                Ok(info) => {
                    if let Some(info) = info {
                        let expect = info.nonce;
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
        if self.commit_result.is_err() {
            return;
        }
        self.state_mut().commit(state);

        // In Ethereum, each transaction includes a miner reward, which would introduce write
        // conflicts in the read-write set if implemented naively, preventing parallel transaction
        // execution. Grevm adopts an optimized approach: it defers miner reward distribution until
        // the transaction commitment phase rather than during execution. This design ensures
        // correct concurrency - even if subsequent transactions access the miner's account, they
        // will read the proper miner state from ParallelState (verified via commit_idx) without
        // creating artificial dependencies.
        let coinbase = self.coinbase;
        assert!(self.state_mut().increment_balances(vec![(coinbase, reward)]).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_context::{
        either::Either,
        result::{Output, ResultGas, SuccessReason},
        transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    };
    use revm_database::EmptyDB;
    use revm_primitives::{Address, B256, Bytes, U256};
    use revm_state::{Account, AccountInfo, AccountStatus};

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
        // `Account` has private `original_info`; build via Default and set the public fields.
        let mut account = Account::default();
        account.info = info;
        account.status = AccountStatus::Touched;
        account
    }

    fn make_result_and_state(caller: Address, post_nonce: u64) -> ResultAndState {
        let mut state: revm_primitives::AddressMap<Account> =
            revm_primitives::AddressMap::default();
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

    fn make_tx_env_with_auth(caller: Address, pre_nonce: u64, authorities: Vec<Address>) -> TxEnv {
        let authorization_list = authorities
            .into_iter()
            .map(|authority| {
                let inner = Authorization {
                    chain_id: U256::ZERO,
                    address: Address::from([0xDE; 20]),
                    nonce: pre_nonce + 1,
                };
                Either::Right(RecoveredAuthorization::new_unchecked(
                    inner,
                    RecoveredAuthority::Valid(authority),
                ))
            })
            .collect();
        TxEnv { tx_type: 4, caller, nonce: pre_nonce, authorization_list, ..Default::default() }
    }

    fn run_commit(state: &UnsafeCell<ParallelState<EmptyDB>>, tx_env: TxEnv, post_nonce: u64) {
        let mut commit = StateAsyncCommit::new(
            Address::ZERO,
            SpecId::PRAGUE,
            0, // basefee: with the tests' zero gas price the reward is 0 regardless
            CommitGuard::new(state),
            false, // disable_nonce_check = false: exercise the assertion path
        );
        commit.init().expect("init");
        commit.commit(0, &tx_env, make_result_and_state(tx_env.caller, post_nonce));
        assert!(
            commit.commit_result().is_ok(),
            "commit_result should be Ok, got {:?}",
            commit.commit_result()
        );
    }

    /// EIP-7702 self-sponsored self-delegation: caller signs the outer tx AND lists itself
    /// as an authority. revm bumps the caller's nonce twice (once for the tx, once for the
    /// auth tuple), so post-state nonce = pre + 2. Without the EIP-7702-aware relaxation,
    /// `assert_eq!(change.info.nonce, expect + 1)` panics with `left: 17, right: 16` — the
    /// exact signature seen on Gravity testnet block 1400868.
    #[test]
    fn self_sponsored_self_delegation_nonce_plus_two_does_not_panic() {
        let caller = Address::from([0xCA; 20]);
        let pre_nonce = 15u64;
        let state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(pre_nonce));
        let state_cell = UnsafeCell::new(state);

        let tx_env = make_tx_env_with_auth(caller, pre_nonce, vec![caller]);
        run_commit(&state_cell, tx_env, pre_nonce + 2);
    }

    /// Two self-auth tuples in the same tx → caller nonce bumps +3. Upper bound of the new
    /// assertion must scale with the count of `authority == caller` tuples.
    #[test]
    fn two_self_auth_tuples_nonce_plus_three_does_not_panic() {
        let caller = Address::from([0xCB; 20]);
        let pre_nonce = 7u64;
        let state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(pre_nonce));
        let state_cell = UnsafeCell::new(state);

        let tx_env = make_tx_env_with_auth(caller, pre_nonce, vec![caller, caller]);
        run_commit(&state_cell, tx_env, pre_nonce + 3);
    }

    /// Self-auth tx where the inner authorization is skipped by revm (e.g. nonce mismatch),
    /// so only the outer tx bumps. Post = pre + 1 must still pass the lower bound.
    #[test]
    fn self_auth_but_only_outer_bump_passes() {
        let caller = Address::from([0xCE; 20]);
        let pre_nonce = 42u64;
        let state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(pre_nonce));
        let state_cell = UnsafeCell::new(state);

        let tx_env = make_tx_env_with_auth(caller, pre_nonce, vec![caller]);
        run_commit(&state_cell, tx_env, pre_nonce + 1);
    }

    /// `compute_reward` must mirror revm's `post_execution::reward_beneficiary`: pre-LONDON the
    /// full effective gas price reaches the coinbase; from LONDON the basefee is burned (EIP-1559).
    /// The mainnet replay harness clamps every pre-Shanghai block to `Merge`, so it never exercises
    /// the pre-LONDON branch — this test pins both branches deterministically.
    #[test]
    fn compute_reward_matches_eip1559_basefee_burn() {
        let state = ParallelState::new(EmptyDB::default(), true, false);
        let state_cell = UnsafeCell::new(state);

        let gas_used = 21_000u64;
        let result = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default().with_total_gas_spent(gas_used),
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        };
        // Legacy tx: effective_gas_price == gas_price regardless of basefee.
        let tx_env = TxEnv { gas_price: 100, gas_limit: gas_used, ..Default::default() };
        let basefee = 10u64;

        // Pre-LONDON: full price to coinbase, basefee NOT subtracted.
        let pre = StateAsyncCommit::new(
            Address::ZERO,
            SpecId::BERLIN,
            basefee,
            CommitGuard::new(&state_cell),
            true,
        );
        assert_eq!(pre.compute_reward(&tx_env, &result), 100u128 * gas_used as u128);

        // LONDON+: basefee burned, only the priority portion reaches the coinbase.
        let post = StateAsyncCommit::new(
            Address::ZERO,
            SpecId::LONDON,
            basefee,
            CommitGuard::new(&state_cell),
            true,
        );
        assert_eq!(post.compute_reward(&tx_env, &result), (100u128 - 10) * gas_used as u128);
    }
}
