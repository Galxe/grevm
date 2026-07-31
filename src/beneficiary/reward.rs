//! Beneficiary reward calculation and settlement policy.

use core::cell::Cell;
use revm::{
    context_interface::{Block, Cfg, Transaction},
    handler::{EvmTr, EvmTrError, FrameResult, post_execution},
    interpreter::Gas,
};
use revm_context::{ContextTr, JournalTr};
use revm_primitives::{U256, hardfork::SpecId};
use revm_state::{AccountInfo, EvmState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeneficiaryMode {
    /// Parallel execution defers reward-only credits, while journal-sensitive interactions remain
    /// immediate.
    Deferred,
    /// Sequential execution applies the beneficiary update before committing the transaction.
    Immediate,
}

impl BeneficiaryMode {
    pub(crate) fn apply<EVM, ERROR>(
        self,
        evm: &mut EVM,
        exec_result: &mut FrameResult,
        deferred_reward: &Cell<Option<DeferredBeneficiaryReward>>,
    ) -> Result<(), ERROR>
    where
        EVM: EvmTr<Context: ContextTr<Journal: JournalTr<State = EvmState>>>,
        ERROR: EvmTrError<EVM>,
    {
        if self == Self::Immediate {
            return post_execution::reward_beneficiary(evm.ctx(), exec_result.gas())
                .map_err(ERROR::from)
        }

        let Some(reward) = BeneficiaryReward::from_gas(evm.ctx_ref(), exec_result.gas()) else {
            return Ok(())
        };
        let beneficiary = evm.ctx_ref().block().beneficiary();
        if reward.is_zero() || evm.ctx_ref().journal().evm_state().contains_key(&beneficiary) {
            post_execution::reward_beneficiary(evm.ctx(), exec_result.gas())
                .map_err(ERROR::from)?;
        } else {
            deferred_reward.set(Some(reward.defer()));
        }
        Ok(())
    }
}

/// A fee reward that revm's post-execution hook would attempt to credit.
///
/// Fee-disabled execution is represented by `None`, while this value may be zero. That distinction
/// preserves revm's zero-reward load and touch behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BeneficiaryReward(U256);

impl BeneficiaryReward {
    /// Compute the exact reward that upstream revm's post-execution hook would attempt to credit.
    fn from_gas<CTX>(context: &CTX, gas: &Gas) -> Option<Self>
    where
        CTX: ContextTr,
    {
        if context.cfg().is_fee_charge_disabled() {
            return None;
        }

        let basefee = context.block().basefee() as u128;
        let effective_gas_price = context.tx().effective_gas_price(basefee);
        let spec: SpecId = context.cfg().spec().clone().into();
        let beneficiary_gas_price = if spec.is_enabled_in(SpecId::LONDON) {
            effective_gas_price.saturating_sub(basefee)
        } else {
            effective_gas_price
        };
        let effective_used = gas.used().saturating_sub(gas.reservoir());

        // Keep arithmetic identical to revm's reward hook.
        Some(Self(U256::from(beneficiary_gas_price * effective_used as u128)))
    }

    /// Whether the reward amount is zero.
    fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// Convert a non-zero reward into the value applied during ordered commit.
    fn defer(self) -> DeferredBeneficiaryReward {
        assert!(!self.is_zero(), "zero rewards must be applied through revm");
        DeferredBeneficiaryReward(self.0)
    }

    /// Return the raw reward amount.
    #[cfg(test)]
    fn amount(self) -> U256 {
        self.0
    }
}

/// A protocol reward proven to be non-zero and therefore safe to defer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeferredBeneficiaryReward(U256);

impl DeferredBeneficiaryReward {
    /// Apply this reward using revm's checked-add semantics.
    ///
    /// Overflow leaves the existing balance unchanged. The non-zero reward materializes an absent
    /// account and preserves every non-balance field of an existing account.
    pub(crate) fn apply_to(self, account: Option<AccountInfo>) -> AccountInfo {
        let mut account = account.unwrap_or_default();
        if let Some(balance) = account.balance.checked_add(self.0) {
            account.balance = balance;
        }
        account
    }

    #[cfg(test)]
    pub(crate) fn for_test(amount: U256) -> Self {
        assert!(!amount.is_zero(), "a deferred reward must be non-zero");
        Self(amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{Context, MainContext, handler::post_execution};
    use revm_context::{BlockEnv, CfgEnv, JournalTr, TxEnv};
    use revm_primitives::Address;

    #[test]
    fn deferred_reward_materializes_an_absent_account() {
        let account = DeferredBeneficiaryReward::for_test(U256::from(7)).apply_to(None);
        assert_eq!(account.balance, U256::from(7));
    }

    #[test]
    fn deferred_reward_preserves_balance_on_overflow() {
        let original = AccountInfo { balance: U256::MAX, ..Default::default() };
        assert_eq!(
            DeferredBeneficiaryReward::for_test(U256::from(1)).apply_to(Some(original.clone())),
            original,
        );
    }

    fn assert_reward_matches_upstream(
        cfg: CfgEnv,
        block: BlockEnv,
        tx: TxEnv,
        gas: Gas,
        expected: Option<U256>,
    ) {
        let beneficiary = block.beneficiary;
        let mut context = Context::mainnet().with_cfg(cfg).with_block(block).with_tx(tx);

        let deferred = BeneficiaryReward::from_gas(&context, &gas).map(|reward| reward.amount());
        post_execution::reward_beneficiary(&mut context, &gas)
            .expect("EmptyDB reward load is infallible");
        let upstream = context
            .journaled_state
            .evm_state()
            .get(&beneficiary)
            .map(|account| account.info.balance);

        assert_eq!(deferred, expected);
        assert_eq!(upstream, expected, "deferred reward drifted from upstream revm");
    }

    #[test]
    fn deferred_reward_matches_upstream_across_forks_and_reservoir_gas() {
        let beneficiary = Address::with_last_byte(0xCB);

        assert_reward_matches_upstream(
            CfgEnv::new_with_spec(SpecId::FRONTIER),
            BlockEnv { beneficiary, basefee: 30, ..Default::default() },
            TxEnv { tx_type: 0, gas_price: 100, ..Default::default() },
            Gas::new_spent_with_reservoir(60, 0),
            Some(U256::from(6_000)),
        );
        assert_reward_matches_upstream(
            CfgEnv::new_with_spec(SpecId::LONDON),
            BlockEnv { beneficiary, basefee: 30, ..Default::default() },
            TxEnv { tx_type: 2, gas_price: 100, gas_priority_fee: Some(5), ..Default::default() },
            Gas::new_spent_with_reservoir(60, 0),
            Some(U256::from(300)),
        );
        // A zero priority fee is still `Some(0)`: upstream loads and touches the beneficiary.
        assert_reward_matches_upstream(
            CfgEnv::new_with_spec(SpecId::LONDON),
            BlockEnv { beneficiary, basefee: 30, ..Default::default() },
            TxEnv { tx_type: 2, gas_price: 30, gas_priority_fee: Some(0), ..Default::default() },
            Gas::new_spent_with_reservoir(60, 0),
            Some(U256::ZERO),
        );
        // EIP-8037 reservoir gas is unused and must not be rewarded.
        assert_reward_matches_upstream(
            CfgEnv::new_with_spec(SpecId::AMSTERDAM),
            BlockEnv { beneficiary, basefee: 30, ..Default::default() },
            TxEnv { tx_type: 2, gas_price: 100, gas_priority_fee: Some(5), ..Default::default() },
            Gas::new_spent_with_reservoir(100, 40),
            Some(U256::from(300)),
        );
    }

    #[test]
    fn fee_disabled_reward_matches_upstream_noop() {
        let beneficiary = Address::with_last_byte(0xCB);
        assert_reward_matches_upstream(
            CfgEnv::new_with_spec(SpecId::PRAGUE).with_disable_fee_charge(true),
            BlockEnv { beneficiary, basefee: 30, ..Default::default() },
            TxEnv { tx_type: 2, gas_price: 100, ..Default::default() },
            Gas::new_spent_with_reservoir(60, 0),
            None,
        );
    }
}
