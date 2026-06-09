use revm::{Database, DatabaseCommit, DatabaseRef};
use revm_context::{
    TxEnv,
    result::{EVMError, ExecutionResult, InvalidTransaction, ResultAndState},
    transaction::{AuthorizationTr, TransactionType},
};
use revm_primitives::Address;

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
        guard: CommitGuard<'a, DB>,
        disable_nonce_check: bool,
    ) -> Self {
        Self { coinbase, results: vec![], guard, commit_result: Ok(()), disable_nonce_check }
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
        let ResultAndState { result, state, lazy_reward } = result_and_state;
        if !self.disable_nonce_check {
            match self.state_ref().basic_ref(tx_env.caller) {
                Ok(info) => {
                    if let Some(info) = info {
                        let expect = info.nonce;
                        if let Some(change) = state.get(&tx_env.caller) {
                            if tx_env.tx_type == TransactionType::Eip7702 as u8 {
                                // EIP-7702 self-sponsored self-delegation can bump the caller's
                                // nonce by more than 1: once for the outer tx, plus once for each
                                // authorization tuple in the same tx whose `authority == caller`
                                // that passes revm's per-tuple validation (chain_id /
                                // existing-code / authority-nonce match / signature). The EIP
                                // documents this broken monotonicity in its Backwards
                                // Compatibility section.
                                //
                                // revm can skip individual tuples without reverting the tx, so
                                // the precise applied count depends on dynamic state at the time
                                // of processing — we can't cheaply re-derive it here without
                                // duplicating revm's auth-list validation. The tightest invariant
                                // we can assert is therefore a range: lower bound = outer tx
                                // alone (any tuple may have been skipped); upper bound = all
                                // self-auth tuples applied.
                                let self_auth_count = tx_env
                                    .authorization_list
                                    .iter()
                                    .filter(|a| a.authority() == Some(tx_env.caller))
                                    .count()
                                    as u64;
                                let min_post = expect + 1;
                                let max_post = expect + 1 + self_auth_count;
                                assert!(
                                    change.info.nonce >= min_post && change.info.nonce <= max_post,
                                    "post-state nonce {} out of range [{}, {}] for caller {:?} \
                                     (tx_type {}, self-auth count {})",
                                    change.info.nonce,
                                    min_post,
                                    max_post,
                                    tx_env.caller,
                                    tx_env.tx_type,
                                    self_auth_count,
                                );
                            } else {
                                assert_eq!(change.info.nonce, expect + 1);
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
        assert!(self.state_mut().increment_balances(vec![(coinbase, lazy_reward)]).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_context::{
        either::Either,
        result::{Output, SuccessReason},
        transaction::{Authorization, RecoveredAuthority, RecoveredAuthorization},
    };
    use revm_database::EmptyDB;
    use revm_primitives::{Address, B256, Bytes, HashMap, U256};
    use revm_state::{Account, AccountInfo, AccountStatus, EvmStorage};

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
            lazy_reward: 0,
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

    /// Auth tuple whose authority is some *other* address must NOT widen the upper bound
    /// for caller. A spurious +2 on caller should still panic — the relaxation is targeted,
    /// not blanket.
    #[test]
    #[should_panic(expected = "post-state nonce")]
    fn foreign_authority_does_not_widen_caller_bound() {
        let caller = Address::from([0xCC; 20]);
        let other = Address::from([0xAA; 20]);
        let pre_nonce = 3u64;
        let state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(pre_nonce));
        let state_cell = UnsafeCell::new(state);

        let tx_env = make_tx_env_with_auth(caller, pre_nonce, vec![other]);
        // Foreign authority → max_post for caller is still pre + 1. Post = pre + 2 must panic.
        run_commit(&state_cell, tx_env, pre_nonce + 2);
    }

    /// Non-7702 path: `tx_type != 4`, post-state nonce = +2 → must still panic via the
    /// original strict equality (regression guard for legacy / 1559 / 4844 txs — panic
    /// message format must stay bit-identical to upstream `assert_eq!`).
    #[test]
    #[should_panic(expected = "left == right")]
    fn legacy_tx_plus_two_still_panics() {
        let caller = Address::from([0xCD; 20]);
        let pre_nonce = 1u64;
        let state = ParallelState::new(EmptyDB::default(), true, false);
        state.insert_account(caller, make_account_info(pre_nonce));
        let state_cell = UnsafeCell::new(state);

        let tx_env = TxEnv { caller, nonce: pre_nonce, ..Default::default() };
        run_commit(&state_cell, tx_env, pre_nonce + 2);
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
}
