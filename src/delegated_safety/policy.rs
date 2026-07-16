use ahash::AHashMap as HashMap;
use revm_context::{Block, BlockEnv, Transaction, TxEnv};
use revm_primitives::{Address, U256};
use std::sync::Arc;

use crate::TxId;

/// Chain-controlled delegated execution safety configuration.
///
/// The default is deliberately disabled so existing callers do not change consensus semantics until
/// Gravity wires this to an activation rule and a protocol-level reserve cap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegatedSafetyConfig {
    /// Enables grevm-local delegated CREATE/CREATE2 blocking and reserve-balance enforcement.
    pub enabled: bool,
    /// Protocol reserve cap used by the fixed-floor part of the policy.
    pub max_reserve_balance: U256,
}

impl DelegatedSafetyConfig {
    /// Disabled configuration preserving upstream revm semantics.
    pub const fn disabled() -> Self {
        Self { enabled: false, max_reserve_balance: U256::ZERO }
    }

    /// Enables delegated safety with a chain-defined maximum reserve balance.
    pub const fn enabled(max_reserve_balance: U256) -> Self {
        Self { enabled: true, max_reserve_balance }
    }
}

impl Default for DelegatedSafetyConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReservePlanError {
    pub(crate) txid: TxId,
}

/// Reverse suffix budget for each sender in the current block.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReservePlan {
    accounts: HashMap<Address, AccountReserveSchedule>,
}

#[derive(Clone, Debug, Default)]
struct AccountReserveSchedule {
    txids: Vec<TxId>,
    need_before: Vec<U256>,
}

impl ReservePlan {
    pub(crate) fn build(txs: &[TxEnv], block: &BlockEnv) -> Result<Self, ReservePlanError> {
        let basefee = block.basefee as u128;
        let blob_gasprice = block.blob_gasprice().unwrap_or_default();
        let mut raw: HashMap<Address, Vec<(TxId, U256, U256)>> = HashMap::new();

        for (txid, tx) in txs.iter().enumerate() {
            let max_spending = tx.max_balance_spending().map_err(|_| ReservePlanError { txid })?;
            let effective_spending = tx
                .effective_balance_spending(basefee, blob_gasprice)
                .map_err(|_| ReservePlanError { txid })?;
            raw.entry(tx.caller).or_default().push((txid, max_spending, effective_spending));
        }

        let mut accounts = HashMap::with_capacity(raw.len());
        for (address, entries) in raw {
            let mut schedule = AccountReserveSchedule {
                txids: Vec::with_capacity(entries.len()),
                need_before: Vec::with_capacity(entries.len()),
            };
            let mut need_after = U256::ZERO;

            for (txid, max_spending, effective_spending) in entries.iter().rev() {
                let effective_with_suffix = effective_spending
                    .checked_add(need_after)
                    .ok_or(ReservePlanError { txid: *txid })?;
                let need_before = (*max_spending).max(effective_with_suffix);
                schedule.txids.push(*txid);
                schedule.need_before.push(need_before);
                need_after = need_before;
            }

            schedule.txids.reverse();
            schedule.need_before.reverse();
            accounts.insert(address, schedule);
        }

        Ok(Self { accounts })
    }

    pub(crate) fn required_after(&self, txid: TxId, address: Address) -> U256 {
        let Some(schedule) = self.accounts.get(&address) else {
            return U256::ZERO;
        };
        match schedule.txids.partition_point(|candidate| *candidate <= txid) {
            index if index < schedule.need_before.len() => schedule.need_before[index],
            _ => U256::ZERO,
        }
    }
}

pub(crate) fn actual_charged_fee<T: Transaction>(tx: &T, block: &BlockEnv, gas_used: u64) -> U256 {
    let gas_fee = U256::from(tx.effective_gas_price(block.basefee as u128))
        .saturating_mul(U256::from(gas_used));

    if tx.tx_type() == revm_context::TransactionType::Eip4844 as u8 {
        let blob_fee = U256::from(block.blob_gasprice().unwrap_or_default())
            .saturating_mul(U256::from(tx.total_blob_gas()));
        gas_fee.saturating_add(blob_fee)
    } else {
        gas_fee
    }
}

pub(crate) fn required_balance(
    config: &DelegatedSafetyConfig,
    plan: &ReservePlan,
    txid: TxId,
    address: Address,
    original_balance: U256,
    is_sender: bool,
    charged_fee: U256,
) -> U256 {
    let mut floor = config.max_reserve_balance.min(original_balance);
    if is_sender {
        floor = floor.saturating_sub(charged_fee);
    }
    floor.max(plan.required_after(txid, address))
}

pub(crate) type SharedReservePlan = Arc<std::sync::OnceLock<Result<ReservePlan, ReservePlanError>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use revm_context::{TransactionType, TxEnv};
    use revm_primitives::{TxKind, address};

    #[test]
    fn reserve_plan_uses_max_headroom_without_double_counting_refunds() {
        let caller = address!("00000000000000000000000000000000000000aa");
        let other = address!("00000000000000000000000000000000000000bb");
        let mut block = BlockEnv::default();
        block.basefee = 10;

        let tx0 = TxEnv {
            tx_type: TransactionType::Eip1559 as u8,
            caller,
            kind: TxKind::Call(other),
            value: U256::from(100),
            gas_limit: 10,
            gas_price: 20,
            gas_priority_fee: Some(1),
            ..TxEnv::default()
        };
        let tx1 = TxEnv {
            tx_type: TransactionType::Eip1559 as u8,
            caller,
            kind: TxKind::Call(other),
            value: U256::from(7),
            gas_limit: 5,
            gas_price: 20,
            gas_priority_fee: Some(1),
            ..TxEnv::default()
        };

        let plan = ReservePlan::build(&[tx0, tx1], &block).unwrap();

        let schedule = plan.accounts.get(&caller).unwrap();
        // tx1 needs max(5*20+7, 5*(10+1)+7) = 107 before it starts.
        assert_eq!(schedule.need_before[1], U256::from(107));
        assert_eq!(plan.required_after(0, caller), U256::from(107));
        // Before tx0, the real suffix after tx0 is tx1's need plus tx0's effective spend:
        // max(10*20+100, 10*(10+1)+100 + 107) = 317.
        assert_eq!(schedule.need_before[0], U256::from(317));
    }
}
