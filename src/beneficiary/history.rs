//! Block-scoped multi-version history owned by the beneficiary aggregate.
//!
//! Every transaction owns exactly one preallocated entry because every transaction may produce a
//! protocol reward. An entry starts as an estimate and is replaced, rather than accumulated, by
//! each execution incarnation. Readers walk entries backwards until an absolute snapshot or the
//! immutable block-start anchor is reached.
//!
//! This component owns the beneficiary history state machine: effect derivation, publication,
//! resolution, and validation. Callers remain responsible for publishing ordinary MV-memory writes
//! before recording an exact beneficiary entry. Consequently, observing an exact entry is the
//! publication boundary for the rest of that incarnation.

use super::DeferredBeneficiaryReward;
use crate::{TxId, TxVersion, account::FinalizedAccount};
use parking_lot::RwLock;
use revm_state::{Account, AccountInfo};

/// The complete version chain used to resolve one beneficiary read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BeneficiaryReadVersion {
    /// Contributing in-block writes, ordered newest first.
    origins: Vec<TxVersion>,
}

impl BeneficiaryReadVersion {
    /// Return the newest contributing transaction, if the read has an in-block dependency.
    pub(crate) fn latest_dependency(&self) -> Option<TxId> {
        self.origins.first().map(|version| version.txid)
    }
}

/// An exact beneficiary account read and the versions from which it was reconstructed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BeneficiaryRead {
    account: Option<AccountInfo>,
    version: BeneficiaryReadVersion,
}

impl BeneficiaryRead {
    /// Consume the read and return its account value and validation version.
    pub(crate) fn into_parts(self) -> (Option<AccountInfo>, BeneficiaryReadVersion) {
        (self.account, self.version)
    }
}

/// Result of validating a previously resolved beneficiary read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BeneficiaryValidation {
    valid: bool,
    /// Scheduling hint for the predecessor on which the reader currently depends.
    ///
    /// When validation encounters an estimate this is the estimate writer, because waiting for an
    /// already-exact newer reward would not make progress. Otherwise it is the newest exact
    /// origin.
    dependency: Option<TxId>,
}

impl BeneficiaryValidation {
    /// Return whether the complete origin chain still matches the earlier read.
    pub(crate) fn is_valid(&self) -> bool {
        self.valid
    }

    /// Return the predecessor that is currently the best scheduling dependency.
    pub(crate) fn dependency(&self) -> Option<TxId> {
        self.dependency
    }
}

/// One transaction's exact effect on beneficiary account history.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BeneficiaryEffect {
    /// The transaction leaves the beneficiary account unchanged.
    Unchanged,
    /// Apply a deferred protocol reward to the preceding value.
    Reward(DeferredBeneficiaryReward),
    /// Replace the preceding value and terminate the reward chain.
    Snapshot(Option<AccountInfo>),
}

impl BeneficiaryEffect {
    fn from_execution(
        deferred_reward: Option<DeferredBeneficiaryReward>,
        account: Option<&Account>,
    ) -> Self {
        if let Some(reward) = deferred_reward {
            assert!(
                account.is_none(),
                "a deferred reward must not accompany a beneficiary state write",
            );
            return Self::Reward(reward)
        }

        match account.map(FinalizedAccount::from) {
            None | Some(FinalizedAccount::Unchanged) => Self::Unchanged,
            Some(FinalizedAccount::Deleted) => Self::Snapshot(None),
            Some(FinalizedAccount::Created(info) | FinalizedAccount::Updated(info)) => {
                Self::Snapshot(Some(info.clone()))
            }
        }
    }
}

#[derive(Clone, Debug)]
enum EntryValue {
    Estimate,
    Exact(BeneficiaryEffect),
}

#[derive(Clone, Debug)]
struct EntryState {
    incarnation: usize,
    value: EntryValue,
}

/// One independently synchronized transaction entry in the history.
#[derive(Debug)]
struct HistoryEntry {
    state: RwLock<EntryState>,
}

impl HistoryEntry {
    fn estimate() -> Self {
        Self { state: RwLock::new(EntryState { incarnation: 0, value: EntryValue::Estimate }) }
    }

    fn record_exact(&self, incarnation: usize, effect: BeneficiaryEffect) -> bool {
        self.record(incarnation, EntryValue::Exact(effect))
    }

    fn record_estimate(&self, incarnation: usize) -> bool {
        self.record(incarnation, EntryValue::Estimate)
    }

    /// Record the first publication for a newer execution incarnation.
    fn record(&self, incarnation: usize, value: EntryValue) -> bool {
        let mut state = self.state.write();
        if incarnation <= state.incarnation {
            return false;
        }

        *state = EntryState { incarnation, value };
        true
    }

    /// Invalidate only the exact incarnation that validation inspected.
    fn invalidate(&self, incarnation: usize) -> bool {
        let mut state = self.state.write();
        if state.incarnation != incarnation {
            return false;
        }
        if matches!(&state.value, EntryValue::Exact(_)) {
            state.value = EntryValue::Estimate;
        }
        true
    }

    /// Copy one entry while holding only its own read lock.
    fn snapshot(&self) -> EntryState {
        self.state.read().clone()
    }
}

#[derive(Debug)]
struct HistoryScan {
    base: Option<AccountInfo>,
    rewards_newest_first: Vec<DeferredBeneficiaryReward>,
    version: BeneficiaryReadVersion,
}

impl HistoryScan {
    /// Resolve the collected rewards in transaction order.
    fn resolve(self) -> BeneficiaryRead {
        // Checked addition has to occur in transaction order. Combining rewards first is not
        // equivalent when one individual addition overflows.
        let account = self
            .rewards_newest_first
            .into_iter()
            .rev()
            .fold(self.base, |account, reward| Some(reward.apply_to(account)));
        BeneficiaryRead { account, version: self.version }
    }

    fn validate(self, expected: &BeneficiaryReadVersion) -> BeneficiaryValidation {
        let dependency = self.version.latest_dependency();
        BeneficiaryValidation { valid: self.version == *expected, dependency }
    }
}

/// Per-block beneficiary account history.
///
/// The block-start anchor never changes. Keeping it separate from the mutable committed cache
/// prevents a reader from observing a cache mutation and then applying the same transaction's
/// reward a second time.
#[derive(Debug)]
pub(crate) struct BeneficiaryHistory {
    block_anchor: Option<AccountInfo>,
    entries: Vec<HistoryEntry>,
}

impl BeneficiaryHistory {
    /// Create a history with one incarnation-zero estimate for every transaction.
    pub(crate) fn new(block_anchor: Option<AccountInfo>, block_size: usize) -> Self {
        Self { block_anchor, entries: (0..block_size).map(|_| HistoryEntry::estimate()).collect() }
    }

    /// Resolve the beneficiary account immediately before `txid`.
    ///
    /// The scan stops at the first snapshot. Otherwise it reaches the immutable block-start
    /// anchor. An error contains the first actionable estimate encountered while walking
    /// backwards.
    pub(crate) fn resolve_before(&self, txid: TxId) -> Result<BeneficiaryRead, TxId> {
        self.scan_before(txid).map(HistoryScan::resolve)
    }

    /// Record one execution incarnation's exact beneficiary effect.
    ///
    /// Returns `false` without changing the entry if a same-or-newer incarnation is already
    /// present. This prevents a stale execution from resurrecting data invalidated by validation.
    pub(crate) fn record_execution(
        &self,
        tx_version: &TxVersion,
        deferred_reward: Option<DeferredBeneficiaryReward>,
        account: Option<&Account>,
    ) -> bool {
        self.record_effect(tx_version, BeneficiaryEffect::from_execution(deferred_reward, account))
    }

    fn record_effect(&self, tx_version: &TxVersion, effect: BeneficiaryEffect) -> bool {
        self.entry(tx_version.txid).record_exact(tx_version.incarnation, effect)
    }

    /// Record an unresolved marker for a failed or conflicting execution attempt.
    pub(crate) fn record_estimate(&self, tx_version: &TxVersion) -> bool {
        self.entry(tx_version.txid).record_estimate(tx_version.incarnation)
    }

    /// Replace the exact effect of `tx_version` with an estimate.
    ///
    /// The incarnation comparison is mandatory: a delayed validation failure from incarnation
    /// `n` must not invalidate data already published by incarnation `n + 1`.
    pub(crate) fn invalidate(&self, tx_version: &TxVersion) -> bool {
        self.entry(tx_version.txid).invalidate(tx_version.incarnation)
    }

    /// Validate an earlier read against every currently contributing origin.
    ///
    /// Comparing only the newest writer is insufficient for a reward chain: an older reward may
    /// have changed incarnation while the newest one stayed unchanged.
    pub(crate) fn validate(
        &self,
        txid: TxId,
        expected: &BeneficiaryReadVersion,
    ) -> BeneficiaryValidation {
        match self.scan_before(txid) {
            Ok(scan) => scan.validate(expected),
            Err(blocker) => BeneficiaryValidation { valid: false, dependency: Some(blocker) },
        }
    }

    fn scan_before(&self, txid: TxId) -> Result<HistoryScan, TxId> {
        assert!(
            txid <= self.entries.len(),
            "beneficiary reader transaction {txid} is outside block of {} transactions",
            self.entries.len()
        );

        let mut origins = Vec::new();
        let mut rewards_newest_first = Vec::new();

        for writer in (0..txid).rev() {
            let EntryState { incarnation, value } = self.entries[writer].snapshot();
            let effect = match value {
                EntryValue::Estimate => return Err(writer),
                EntryValue::Exact(effect) => effect,
            };

            origins.push(TxVersion::new(writer, incarnation));
            match effect {
                BeneficiaryEffect::Unchanged => {}
                BeneficiaryEffect::Reward(reward) => rewards_newest_first.push(reward),
                BeneficiaryEffect::Snapshot(account) => {
                    return Ok(HistoryScan {
                        base: account,
                        rewards_newest_first,
                        version: BeneficiaryReadVersion { origins },
                    });
                }
            }
        }

        Ok(HistoryScan {
            base: self.block_anchor.clone(),
            rewards_newest_first,
            version: BeneficiaryReadVersion { origins },
        })
    }

    fn entry(&self, txid: TxId) -> &HistoryEntry {
        self.entries.get(txid).unwrap_or_else(|| {
            panic!(
                "beneficiary writer transaction {txid} is outside block of {} transactions",
                self.entries.len()
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm_primitives::U256;

    fn version(txid: TxId, incarnation: usize) -> TxVersion {
        TxVersion::new(txid, incarnation)
    }

    fn account(balance: U256) -> AccountInfo {
        AccountInfo { balance, ..Default::default() }
    }

    fn reward(amount: U256) -> DeferredBeneficiaryReward {
        DeferredBeneficiaryReward::for_test(amount)
    }

    fn record_reward(history: &BeneficiaryHistory, tx_version: &TxVersion, amount: U256) -> bool {
        history.record_execution(tx_version, Some(reward(amount)), None)
    }

    fn record_snapshot(
        history: &BeneficiaryHistory,
        tx_version: &TxVersion,
        account: Option<AccountInfo>,
    ) -> bool {
        history.record_effect(tx_version, BeneficiaryEffect::Snapshot(account))
    }

    fn resolved(
        result: Result<BeneficiaryRead, TxId>,
    ) -> (Option<AccountInfo>, BeneficiaryReadVersion) {
        result
            .unwrap_or_else(|blocker| {
                panic!("expected an exact account, blocked by transaction {blocker}")
            })
            .into_parts()
    }

    #[test]
    fn estimates_are_preseeded_and_nearest_relevant_writer_blocks() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 4);

        // A transaction never depends on its own predeclared writer.
        let (resolved_account, read_version) = resolved(history.resolve_before(0));
        assert_eq!(resolved_account.unwrap().balance, U256::from(10));
        assert!(read_version.origins.is_empty());

        assert_eq!(history.resolve_before(3), Err(2));

        // An exact snapshot is a checkpoint: older estimates cannot affect this read.
        assert!(record_snapshot(&history, &version(2, 1), Some(account(U256::from(20))),));
        let (account, read_version) = resolved(history.resolve_before(3));
        assert_eq!(account.unwrap().balance, U256::from(20));
        assert_eq!(read_version.origins, vec![version(2, 1)]);
    }

    #[test]
    fn reward_chain_resolves_from_anchor_and_records_every_origin() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 3);
        assert!(record_reward(&history, &version(0, 1), U256::from(2)));
        assert!(record_reward(&history, &version(1, 1), U256::from(3)));

        let (account, read_version) = resolved(history.resolve_before(2));
        assert_eq!(account.unwrap().balance, U256::from(15));
        assert_eq!(read_version.origins, vec![version(1, 1), version(0, 1)]);
        assert_eq!(read_version.latest_dependency(), Some(1));
    }

    #[test]
    fn snapshot_none_cuts_older_estimates_and_later_reward_recreates_account() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(99))), 4);
        assert!(record_snapshot(&history, &version(1, 1), None));
        assert!(record_reward(&history, &version(2, 1), U256::from(7)));

        let (account, read_version) = resolved(history.resolve_before(3));
        assert_eq!(account.unwrap().balance, U256::from(7));
        assert_eq!(read_version.origins, vec![version(2, 1), version(1, 1)]);
    }

    #[test]
    fn retry_replaces_write_and_stale_record_or_invalidation_cannot_win() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 1);
        assert!(record_reward(&history, &version(0, 1), U256::from(4)));
        assert!(history.invalidate(&version(0, 1)));
        assert_eq!(history.resolve_before(1), Err(0));

        assert!(record_reward(&history, &version(0, 2), U256::from(7)));
        assert!(!record_reward(&history, &version(0, 1), U256::from(100)));
        assert!(!history.invalidate(&version(0, 1)));

        let (account, read_version) = resolved(history.resolve_before(1));
        assert_eq!(account.unwrap().balance, U256::from(17));
        assert_eq!(read_version.origins, vec![version(0, 2)]);
    }

    #[test]
    fn execution_conflict_keeps_predeclared_writer_as_estimate() {
        let history = BeneficiaryHistory::new(None, 1);
        assert!(history.record_estimate(&version(0, 1)));
        assert_eq!(history.resolve_before(1), Err(0));
        assert!(history.invalidate(&version(0, 1)));
    }

    #[test]
    fn validation_checks_older_reward_incarnations_not_only_latest_writer() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 2);
        assert!(record_reward(&history, &version(0, 1), U256::from(1)));
        assert!(record_reward(&history, &version(1, 1), U256::from(2)));
        let (_, expected) = resolved(history.resolve_before(2));

        // The newest origin (tx 1) is unchanged, but tx 0 has a new incarnation.
        assert!(record_reward(&history, &version(0, 2), U256::from(9)));
        let validation = history.validate(2, &expected);
        assert!(!validation.is_valid());
        assert_eq!(validation.dependency(), Some(1));
    }

    #[test]
    fn validation_reports_estimate_as_actionable_dependency() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 2);
        assert!(record_reward(&history, &version(0, 1), U256::from(1)));
        assert!(record_reward(&history, &version(1, 1), U256::from(2)));
        let (_, expected) = resolved(history.resolve_before(2));

        assert!(history.invalidate(&version(0, 1)));
        let validation = history.validate(2, &expected);
        assert!(!validation.is_valid());
        assert_eq!(validation.dependency(), Some(0));
    }

    #[test]
    fn reward_overflow_is_evaluated_in_transaction_order() {
        let history = BeneficiaryHistory::new(Some(account(U256::MAX - U256::from(1))), 2);
        assert!(record_reward(&history, &version(0, 1), U256::from(2)));
        assert!(record_reward(&history, &version(1, 1), U256::from(1)));

        let (account, _) = resolved(history.resolve_before(2));
        // tx 0 overflows and leaves MAX-1 unchanged; tx 1 then advances it to MAX.
        assert_eq!(account.unwrap().balance, U256::MAX);
    }

    #[test]
    fn unchanged_effect_is_an_exact_noop_for_an_absent_anchor() {
        let history = BeneficiaryHistory::new(None, 1);
        assert!(history.record_execution(&version(0, 1), None, None));

        let (account, read_version) = resolved(history.resolve_before(1));
        assert!(account.is_none());
        assert_eq!(read_version.origins, vec![version(0, 1)]);
    }

    #[test]
    fn same_incarnation_estimate_cannot_be_completed_later() {
        let history = BeneficiaryHistory::new(None, 1);
        let tx_version = version(0, 1);
        assert!(history.record_estimate(&tx_version));
        assert!(!record_reward(&history, &tx_version, U256::from(1)));
        assert_eq!(history.resolve_before(1), Err(0));
    }

    #[test]
    fn stale_estimate_cannot_hide_a_newer_exact_effect() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 1);
        assert!(record_reward(&history, &version(0, 2), U256::from(7)));
        assert!(!history.record_estimate(&version(0, 1)));

        let (account, read_version) = resolved(history.resolve_before(1));
        assert_eq!(account.unwrap().balance, U256::from(17));
        assert_eq!(read_version.origins, vec![version(0, 2)]);
    }

    #[test]
    fn valid_validation_reports_the_latest_exact_dependency() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 2);
        assert!(record_reward(&history, &version(0, 1), U256::from(1)));
        assert!(record_reward(&history, &version(1, 1), U256::from(2)));
        let (_, expected) = resolved(history.resolve_before(2));

        let validation = history.validate(2, &expected);
        assert!(validation.is_valid());
        assert_eq!(validation.dependency(), Some(1));
    }

    #[test]
    fn snapshot_keeps_read_valid_when_an_older_entry_changes() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 2);
        assert!(record_reward(&history, &version(0, 1), U256::from(1)));
        assert!(record_snapshot(&history, &version(1, 1), Some(account(U256::from(20))),));
        let (_, expected) = resolved(history.resolve_before(2));

        assert!(record_reward(&history, &version(0, 2), U256::from(9)));
        let validation = history.validate(2, &expected);
        assert!(validation.is_valid());
        assert_eq!(validation.dependency(), Some(1));
    }

    #[test]
    fn finalized_account_is_recorded_as_an_absolute_snapshot() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 1);
        let mut updated = Account::from(account(U256::from(25)));
        updated.mark_touch();
        assert!(history.record_execution(&version(0, 1), None, Some(&updated)));

        let (account, read_version) = resolved(history.resolve_before(1));
        assert_eq!(account.unwrap().balance, U256::from(25));
        assert_eq!(read_version.origins, vec![version(0, 1)]);
    }

    #[test]
    fn finalized_deletion_is_recorded_as_an_absent_snapshot() {
        let history = BeneficiaryHistory::new(Some(account(U256::from(10))), 1);
        let mut deleted = Account::from(account(U256::from(25)));
        deleted.mark_touch();
        deleted.mark_selfdestruct();
        assert!(history.record_execution(&version(0, 1), None, Some(&deleted)));

        let (account, read_version) = resolved(history.resolve_before(1));
        assert!(account.is_none());
        assert_eq!(read_version.origins, vec![version(0, 1)]);
    }
}
