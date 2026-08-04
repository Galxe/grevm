//! Beneficiary reward policy and block-scoped state coordination.
//!
//! Parallel execution applies a reward immediately when the beneficiary is already present in the
//! transaction journal (or when a zero reward must preserve revm's touch semantics). Otherwise it
//! records a non-zero reward for ordered commit. Keeping that decision in the handler preserves
//! revm's journal ordering without making every transaction load the beneficiary account.

mod history;
mod reward;

use crate::{TxId, TxVersion};
use history::BeneficiaryHistory;

use revm_context::result::ResultAndState;
use revm_primitives::Address;
use revm_state::{AccountInfo, EvmState};

#[cfg(test)]
use revm_primitives::U256;

pub(crate) use history::{BeneficiaryRead, BeneficiaryReadVersion, BeneficiaryValidation};
pub(crate) use reward::{BeneficiaryMode, DeferredBeneficiaryReward};

/// The beneficiary account and its block-scoped speculative history.
///
/// Owning the address and history together prevents workers from accidentally resolving one
/// beneficiary while filtering writes for another.
#[derive(Debug)]
pub(crate) struct Beneficiary {
    address: Address,
    history: BeneficiaryHistory,
}

impl Beneficiary {
    /// Create a beneficiary backed by the immutable block-start account value.
    pub(crate) fn new(
        address: Address,
        block_anchor: Option<AccountInfo>,
        block_size: usize,
    ) -> Self {
        Self { address, history: BeneficiaryHistory::new(block_anchor, block_size) }
    }

    /// Whether `address` identifies this block's beneficiary.
    pub(crate) fn matches(&self, address: Address) -> bool {
        self.address == address
    }

    /// Resolve the account immediately before `txid` or return the blocking predecessor.
    pub(crate) fn resolve_before(&self, txid: TxId) -> Result<BeneficiaryRead, TxId> {
        self.history.resolve_before(txid)
    }

    /// Publish the exact beneficiary effect of a successful execution.
    ///
    /// Callers must publish the execution's ordinary MV-memory writes first. This history update
    /// is the publication boundary for the rest of the incarnation.
    pub(crate) fn record_execution(
        &self,
        tx_version: &TxVersion,
        result: &SpeculativeResult,
    ) -> bool {
        let deferred_reward = result.deferred_reward();
        let account = result.state().get(&self.address);
        assert!(
            deferred_reward.is_none() || account.is_none(),
            "a deferred reward must not accompany a beneficiary state write",
        );
        self.history.record_execution(tx_version, deferred_reward, account)
    }

    /// Publish an unresolved marker for a failed or conflicting execution attempt.
    pub(crate) fn record_estimate(&self, tx_version: &TxVersion) -> bool {
        self.history.record_estimate(tx_version)
    }

    /// Invalidate the exact effect produced by this incarnation.
    pub(crate) fn invalidate(&self, tx_version: &TxVersion) -> bool {
        self.history.invalidate(tx_version)
    }

    /// Validate a prior read against the beneficiary's current version chain.
    pub(crate) fn validate(
        &self,
        txid: TxId,
        expected: &BeneficiaryReadVersion,
    ) -> BeneficiaryValidation {
        self.history.validate(txid, expected)
    }

    #[cfg(test)]
    pub(crate) fn record_reward_for_test(&self, tx_version: &TxVersion, amount: U256) -> bool {
        self.history.record_execution(
            tx_version,
            Some(DeferredBeneficiaryReward::for_test(amount)),
            None,
        )
    }
}

/// Successful speculative execution and any non-zero reward left for ordered commit.
#[derive(Debug)]
pub(crate) struct SpeculativeResult {
    result_and_state: ResultAndState,
    deferred_reward: Option<DeferredBeneficiaryReward>,
}

impl SpeculativeResult {
    /// Build a result whose beneficiary effects were applied immediately by revm.
    pub(crate) fn settled(result_and_state: ResultAndState) -> Self {
        Self { result_and_state, deferred_reward: None }
    }

    /// Build a result with one non-zero beneficiary reward reserved for ordered commit.
    pub(crate) fn deferred(
        result_and_state: ResultAndState,
        deferred_reward: DeferredBeneficiaryReward,
    ) -> Self {
        Self { result_and_state, deferred_reward: Some(deferred_reward) }
    }

    /// Finalized state produced by speculative execution.
    pub(crate) fn state(&self) -> &EvmState {
        &self.result_and_state.state
    }

    /// The non-zero reward intentionally omitted from the finalized state.
    pub(crate) fn deferred_reward(&self) -> Option<DeferredBeneficiaryReward> {
        self.deferred_reward
    }

    /// Consume the wrapper at the ordered-commit boundary.
    pub(crate) fn into_commit_parts(self) -> (ResultAndState, Option<DeferredBeneficiaryReward>) {
        (self.result_and_state, self.deferred_reward)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_context::result::{ExecutionResult, Output, ResultGas, SuccessReason};
    use revm_primitives::{Address, Bytes};
    use revm_state::Account;

    #[test]
    #[should_panic(expected = "a deferred reward must not accompany a beneficiary state write")]
    fn beneficiary_rejects_deferred_reward_with_state_write() {
        let beneficiary = Address::with_last_byte(0xCB);
        let mut state = revm_primitives::AddressMap::default();
        state.insert(beneficiary, Account::default());
        let result_and_state = ResultAndState {
            result: ExecutionResult::Success {
                reason: SuccessReason::Stop,
                gas: ResultGas::default(),
                logs: Vec::new(),
                output: Output::Call(Bytes::new()),
            },
            state,
        };

        let result = SpeculativeResult::deferred(
            result_and_state,
            DeferredBeneficiaryReward::for_test(U256::from(1)),
        );
        let beneficiary = Beneficiary::new(beneficiary, None, 1);

        beneficiary.record_execution(&TxVersion::new(0, 0), &result);
    }
}
