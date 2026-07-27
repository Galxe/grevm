use crate::TxId;
use ahash::AHashMap as HashMap;
use dashmap::DashMap;
use metrics::{counter, histogram};
use revm::Database;
use revm_context::{Journal, JournalEntry, Transaction, TxEnv, journaled_state::JournalCheckpoint};
use revm_primitives::{Address, TxKind, U256};
use std::{
    sync::{Arc, OnceLock},
    time::Instant,
};

/// Lazily computes the conservative cost of one account's later block transactions.
///
/// The first query builds one lightweight `caller -> txids` index. A queried account then gets
/// one suffix-sum array; accounts that never transfer value from delegated execution pay no U256
/// calculation or per-account allocation.
#[derive(Debug)]
pub(crate) struct ReservePlanner {
    /// Transactions in consensus order. `TxId` is an index into this vector.
    txs: Arc<Vec<TxEnv>>,
    /// `caller -> ascending TxId list`, initialized by one O(block size) scan.
    sender_index: OnceLock<HashMap<Address, Vec<TxId>>>,
    /// Independently initialized account suffixes shared by parallel workers.
    schedules: DashMap<Address, Arc<OnceLock<AccountReserveSchedule>>>,
}

/// Suffix sums for the transactions sent by one account.
///
/// ```text
/// block order       ... T3(A) ... T8(A) ... T11(A) ...
/// txids                   3         8          11
/// cost_from          C3+C8+C11   C8+C11       C11
///
/// Ci = Ti.max_balance_spending()
/// ```
///
/// `required_after(i)` returns the `cost_from` entry for A's first transaction strictly after
/// `i`. Summing maximum costs is intentionally conservative and keeps the policy aligned with
/// revm's upfront balance check without recreating gravity-reth's execution simulation.
#[derive(Clone, Debug, Default)]
struct AccountReserveSchedule {
    /// Strictly increasing transaction IDs whose top-level caller is this account.
    txids: Vec<TxId>,
    /// Saturating `max_balance_spending()` suffix sums aligned with `txids`.
    cost_from: Vec<U256>,
}

impl ReservePlanner {
    pub(crate) fn new(txs: Arc<Vec<TxEnv>>) -> Self {
        Self { txs, sender_index: OnceLock::new(), schedules: DashMap::new() }
    }

    /// Returns the conservative cost of `address`'s transactions strictly after `txid`.
    ///
    /// A malformed transaction whose own maximum cost overflows is treated as `U256::MAX`.
    /// Suffix addition also saturates. The reserve policy therefore remains conservative without
    /// introducing a second block-fatal error path for a future transaction that normal
    /// validation/filtering will reject.
    pub(crate) fn required_after(&self, txid: TxId, address: Address) -> U256 {
        counter!("grevm.reserve_query_count").increment(1);
        let Some(txids) = self.sender_index().get(&address) else {
            return U256::ZERO;
        };
        if txids.partition_point(|candidate| *candidate <= txid) == txids.len() {
            return U256::ZERO;
        }

        let cell =
            self.schedules.entry(address).or_insert_with(|| Arc::new(OnceLock::new())).clone();
        let schedule = cell.get_or_init(|| {
            let start = Instant::now();
            counter!("grevm.reserve_schedule_build_count").increment(1);
            let schedule = self.build_schedule(txids);
            histogram!("grevm.reserve_schedule_build_time")
                .record(start.elapsed().as_nanos() as f64);
            schedule
        });
        schedule.required_after(txid)
    }

    fn sender_index(&self) -> &HashMap<Address, Vec<TxId>> {
        self.sender_index.get_or_init(|| {
            let start = Instant::now();
            let mut index = HashMap::new();
            for (txid, tx) in self.txs.iter().enumerate() {
                index.entry(tx.caller).or_insert_with(Vec::new).push(txid);
            }
            counter!("grevm.reserve_index_build_count").increment(1);
            histogram!("grevm.reserve_index_build_time").record(start.elapsed().as_nanos() as f64);
            index
        })
    }

    fn build_schedule(&self, txids: &[TxId]) -> AccountReserveSchedule {
        let mut schedule = AccountReserveSchedule {
            txids: txids.to_vec(),
            cost_from: vec![U256::ZERO; txids.len()],
        };
        let mut suffix = U256::ZERO;

        for (index, txid) in txids.iter().copied().enumerate().rev() {
            let cost = self.txs[txid].max_balance_spending().unwrap_or(U256::MAX);
            suffix = suffix.saturating_add(cost);
            schedule.cost_from[index] = suffix;
        }

        schedule
    }

    #[cfg(test)]
    fn is_initialized(&self) -> bool {
        self.sender_index.get().is_some()
    }

    #[cfg(test)]
    fn initialized_accounts(&self) -> usize {
        self.schedules.len()
    }
}

impl AccountReserveSchedule {
    fn required_after(&self, txid: TxId) -> U256 {
        match self.txids.partition_point(|candidate| *candidate <= txid) {
            index if index < self.cost_from.len() => self.cost_from[index],
            _ => U256::ZERO,
        }
    }
}

/// One delegated account whose surviving execution debit needs a reserve check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DelegatedDebit {
    /// EIP-7702 delegation-designator account that lost balance.
    pub(crate) address: Address,
    /// Balance immediately before its first surviving protected debit in this transaction.
    ///
    /// This deliberately includes value credited earlier in the same delegated execution. Once
    /// those funds become spendable by the delegated account, protecting them before a later debit
    /// gives future block transactions the strongest conservative funding guarantee.
    pub(crate) balance_before: U256,
    /// Balance after execution and unused-gas reimbursement.
    pub(crate) final_balance: U256,
}

/// Finds real balance debits made from delegated execution using revm's surviving journal.
///
/// A standard EVM value-moving operation debits its current state account. Therefore, after
/// excluding the root transaction value transfer, a debit whose source still contains an
/// EIP-7702 designator came from that delegated account's execution context. Reverted inner-frame
/// entries are already absent from the journal.
pub(crate) trait ReserveJournalExt {
    fn delegated_debits_since(
        &self,
        checkpoint: JournalCheckpoint,
        tx: &TxEnv,
    ) -> Vec<DelegatedDebit>;
}

impl<DB: Database> ReserveJournalExt for Journal<DB> {
    fn delegated_debits_since(
        &self,
        checkpoint: JournalCheckpoint,
        tx: &TxEnv,
    ) -> Vec<DelegatedDebit> {
        debug_assert!(checkpoint.journal_i <= self.inner.journal.len());
        let entries = &self.inner.journal;
        let mut root_value_pending = !tx.value.is_zero();
        let mut first_debit = HashMap::<Address, usize>::new();

        for (entry_index, entry) in entries.iter().enumerate().skip(checkpoint.journal_i) {
            if root_value_pending && is_root_value_transfer(entry, tx) {
                // `filter_invalid_txs` already sees transaction.value. A delegated EOA may send
                // an ordinary transaction without executing its own delegated code, so this
                // single top-level transfer must not become a reserve candidate.
                root_value_pending = false;
                continue;
            }

            let source = match entry {
                JournalEntry::BalanceTransfer { from, to, balance }
                    if from != to && !balance.is_zero() =>
                {
                    Some(*from)
                }
                JournalEntry::AccountDestroyed { address, had_balance, .. }
                    if !had_balance.is_zero() =>
                {
                    Some(*address)
                }
                _ => None,
            };
            let Some(source) = source else {
                continue;
            };
            let is_delegated = self
                .inner
                .state
                .get(&source)
                .and_then(|account| account.info.code.as_ref())
                .is_some_and(|code| code.is_eip7702());
            if is_delegated {
                first_debit.entry(source).or_insert(entry_index);
            }
        }

        first_debit
            .into_iter()
            .filter_map(|(address, entry_index)| {
                let final_balance = self.inner.state.get(&address)?.info.balance;
                Some(DelegatedDebit {
                    address,
                    balance_before: balance_before_entry(
                        entries,
                        entry_index,
                        address,
                        final_balance,
                    ),
                    final_balance,
                })
            })
            .collect()
    }
}

fn is_root_value_transfer(entry: &JournalEntry, tx: &TxEnv) -> bool {
    let JournalEntry::BalanceTransfer { from, to, balance } = entry else {
        return false;
    };
    if *from != tx.caller || *balance != tx.value {
        return false;
    }
    match tx.kind {
        TxKind::Call(target) => *to == target,
        // The root CREATE transfer precedes init-code execution, so the first matching transfer
        // from the transaction caller is unambiguously the filter-visible transaction value.
        TxKind::Create => true,
    }
}

/// Reconstructs the balance before `entry_index` by undoing the surviving balance journal.
///
/// ```text
/// balance before first delegated debit
///          | debit | ... credits/debits ... | reimburse | final balance
///          <---------------- reverse journal ----------------'
/// ```
///
/// Saturating arithmetic is defensive only; valid revm journal entries reverse exactly.
fn balance_before_entry(
    entries: &[JournalEntry],
    entry_index: usize,
    address: Address,
    final_balance: U256,
) -> U256 {
    let mut balance = final_balance;
    for entry in entries[entry_index..].iter().rev() {
        match entry {
            JournalEntry::BalanceTransfer { from, to, balance: value } => {
                if *from == address && *to != address {
                    balance = balance.saturating_add(*value);
                } else if *to == address && *from != address {
                    balance = balance.saturating_sub(*value);
                }
            }
            JournalEntry::AccountDestroyed { address: destroyed, target, had_balance, .. } => {
                if *destroyed == address {
                    balance = balance.saturating_add(*had_balance);
                } else if *target == address {
                    balance = balance.saturating_sub(*had_balance);
                }
            }
            JournalEntry::BalanceChange { address: changed, old_balance }
                if *changed == address =>
            {
                balance = *old_balance;
            }
            _ => {}
        }
    }
    balance
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::{
        context::JournalTr, context_interface::journaled_state::entry::SelfdestructionRevertStatus,
    };
    use revm_database::EmptyDB;
    use revm_primitives::address;
    use revm_state::{Account, Bytecode};

    fn tx(caller: Address, value: u64, gas_limit: u64, gas_price: u128) -> TxEnv {
        TxEnv {
            caller,
            kind: TxKind::Call(Address::ZERO),
            value: U256::from(value),
            gas_limit,
            gas_price,
            ..Default::default()
        }
    }

    #[test]
    fn planner_is_lazy_and_builds_only_queried_accounts() {
        let a = address!("00000000000000000000000000000000000000aa");
        let b = address!("00000000000000000000000000000000000000bb");
        let planner =
            ReservePlanner::new(Arc::new(vec![tx(a, 0, 10, 2), tx(b, 0, 20, 3), tx(a, 0, 30, 4)]));

        assert!(!planner.is_initialized());
        assert_eq!(planner.required_after(0, a), U256::from(120));
        assert!(planner.is_initialized());
        assert_eq!(planner.initialized_accounts(), 1);
        assert_eq!(planner.required_after(0, b), U256::from(60));
        assert_eq!(planner.initialized_accounts(), 2);
    }

    #[test]
    fn suffix_accumulates_every_future_maximum_cost() {
        let caller = address!("00000000000000000000000000000000000000aa");
        let planner =
            ReservePlanner::new(Arc::new(vec![tx(caller, 100, 10, 20), tx(caller, 7, 5, 20)]));

        assert_eq!(planner.required_after(0, caller), U256::from(107));
        let schedule = planner.schedules.get(&caller).unwrap();
        let schedule = schedule.get().unwrap();
        assert_eq!(schedule.cost_from[0], U256::from(407));
    }

    #[test]
    fn queries_use_logical_txid_not_first_query_order() {
        let caller = address!("00000000000000000000000000000000000000aa");
        let txs = (0..9).map(|_| tx(caller, 0, 1, 1)).collect();
        let planner = ReservePlanner::new(Arc::new(txs));

        assert_eq!(planner.required_after(7, caller), U256::from(1));
        assert_eq!(planner.required_after(2, caller), U256::from(6));
        assert_eq!(planner.required_after(8, caller), U256::ZERO);
    }

    #[test]
    fn planner_saturates_malformed_or_oversized_suffixes() {
        let caller = address!("00000000000000000000000000000000000000aa");
        let planner = ReservePlanner::new(Arc::new(vec![
            tx(Address::ZERO, 0, 1, 1),
            tx(caller, 0, u64::MAX, u128::MAX),
            tx(caller, 0, 1, 1),
        ]));

        assert_eq!(planner.required_after(0, caller), U256::MAX);
    }

    #[test]
    fn journal_selects_only_delegated_debits_and_excludes_root_value() {
        let delegated = address!("00000000000000000000000000000000000000aa");
        let ordinary = address!("00000000000000000000000000000000000000bb");
        let receiver = address!("00000000000000000000000000000000000000cc");
        let mut journal = Journal::<EmptyDB>::new(EmptyDB::default());
        let mut delegated_account = Account::default();
        delegated_account.info.balance = U256::from(100);
        delegated_account.info.code = Some(Bytecode::new_eip7702(receiver));
        journal.inner.state.insert(delegated, delegated_account);
        let mut ordinary_account = Account::default();
        ordinary_account.info.balance = U256::from(100);
        journal.inner.state.insert(ordinary, ordinary_account);
        journal.inner.state.insert(receiver, Account::default());

        let checkpoint = journal.checkpoint();
        journal.inner.journal.extend([
            // Filter-visible root transaction value: excluded even though caller is delegated.
            JournalEntry::BalanceTransfer {
                from: delegated,
                to: ordinary,
                balance: U256::from(10),
            },
            // Ordinary contract debit: not a delegated execution context.
            JournalEntry::BalanceTransfer { from: ordinary, to: receiver, balance: U256::from(20) },
            // Delegated execution debit: the only candidate.
            JournalEntry::BalanceTransfer {
                from: delegated,
                to: receiver,
                balance: U256::from(30),
            },
        ]);
        journal.inner.state.get_mut(&delegated).unwrap().info.balance = U256::from(60);
        journal.inner.state.get_mut(&ordinary).unwrap().info.balance = U256::from(90);
        journal.inner.state.get_mut(&receiver).unwrap().info.balance = U256::from(50);
        let root_tx = TxEnv {
            caller: delegated,
            kind: TxKind::Call(ordinary),
            value: U256::from(10),
            ..Default::default()
        };

        assert_eq!(
            journal.delegated_debits_since(checkpoint, &root_tx),
            vec![DelegatedDebit {
                address: delegated,
                balance_before: U256::from(90),
                final_balance: U256::from(60),
            }]
        );
    }

    #[test]
    fn selfdestruct_and_later_credit_restore_the_pre_debit_balance() {
        let delegated = address!("00000000000000000000000000000000000000aa");
        let receiver = address!("00000000000000000000000000000000000000bb");
        let mut journal = Journal::<EmptyDB>::new(EmptyDB::default());
        let mut account = Account::default();
        account.info.code = Some(Bytecode::new_eip7702(receiver));
        account.info.balance = U256::from(5);
        journal.inner.state.insert(delegated, account);
        journal.inner.state.insert(receiver, Account::default());
        let checkpoint = journal.checkpoint();
        journal.inner.journal.extend([
            JournalEntry::AccountDestroyed {
                address: delegated,
                target: receiver,
                destroyed_status: SelfdestructionRevertStatus::GloballySelfdestroyed,
                had_balance: U256::from(5),
            },
            JournalEntry::BalanceTransfer { from: receiver, to: delegated, balance: U256::from(2) },
        ]);
        journal.inner.state.get_mut(&delegated).unwrap().info.balance = U256::from(2);
        journal.inner.state.get_mut(&receiver).unwrap().info.balance = U256::from(3);

        assert_eq!(
            journal.delegated_debits_since(checkpoint, &TxEnv::default()),
            vec![DelegatedDebit {
                address: delegated,
                balance_before: U256::from(5),
                final_balance: U256::from(2),
            }]
        );
    }

    #[test]
    fn credit_before_first_debit_is_included_in_the_protected_balance() {
        let delegated = address!("00000000000000000000000000000000000000aa");
        let funder = address!("00000000000000000000000000000000000000bb");
        let receiver = address!("00000000000000000000000000000000000000cc");
        let mut journal = Journal::<EmptyDB>::new(EmptyDB::default());
        let mut account = Account::default();
        account.info.code = Some(Bytecode::new_eip7702(receiver));
        account.info.balance = U256::from(100);
        journal.inner.state.insert(delegated, account);
        journal.inner.state.insert(funder, Account::default());
        journal.inner.state.insert(receiver, Account::default());
        let checkpoint = journal.checkpoint();
        journal.inner.journal.extend([
            // The delegated account first receives 1,000, then spends exactly that credit.
            JournalEntry::BalanceTransfer {
                from: funder,
                to: delegated,
                balance: U256::from(1_000),
            },
            JournalEntry::BalanceTransfer {
                from: delegated,
                to: receiver,
                balance: U256::from(1_000),
            },
        ]);
        journal.inner.state.get_mut(&delegated).unwrap().info.balance = U256::from(100);

        assert_eq!(
            journal.delegated_debits_since(checkpoint, &TxEnv::default()),
            vec![DelegatedDebit {
                address: delegated,
                balance_before: U256::from(1_100),
                final_balance: U256::from(100),
            }]
        );
    }

    #[test]
    fn reverted_native_entries_need_no_second_undo_stack() {
        let delegated = address!("00000000000000000000000000000000000000aa");
        let receiver = address!("00000000000000000000000000000000000000bb");
        let mut journal = Journal::<EmptyDB>::new(EmptyDB::default());
        let mut sender = Account::default();
        sender.info.balance = U256::from(1);
        sender.info.code = Some(Bytecode::new_eip7702(receiver));
        journal.inner.state.insert(delegated, sender);
        journal.inner.state.insert(receiver, Account::default());
        let execution = journal.checkpoint();
        let inner = journal.checkpoint();
        assert_eq!(journal.transfer(delegated, receiver, U256::from(1)).unwrap(), None);
        journal.checkpoint_revert(inner);

        assert!(journal.delegated_debits_since(execution, &TxEnv::default()).is_empty());
    }
}
