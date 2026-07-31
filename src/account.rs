//! Finalized journal-account lifecycle classification.

use revm_state::{Account, AccountInfo};

/// The consensus-visible state change represented by one finalized journal account.
///
/// Revm has already normalized fork-specific empty-account and SELFDESTRUCT behavior by the time
/// it returns the account. Keeping the classification here lets every consumer share that result
/// without re-deriving hardfork rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinalizedAccount<'a> {
    /// Merely loaded accounts do not write any state.
    Unchanged,
    /// Actual SELFDESTRUCT or a finalized EIP-161 empty-account deletion.
    Deleted,
    /// A newly created account. Creation also resets all prior storage.
    Created(&'a AccountInfo),
    /// An update to an existing account.
    Updated(&'a AccountInfo),
}

impl<'a> From<&'a Account> for FinalizedAccount<'a> {
    fn from(account: &'a Account) -> Self {
        if !account.is_touched() {
            Self::Unchanged
        } else if account.is_selfdestructed() {
            Self::Deleted
        } else if account.is_created() {
            Self::Created(&account.info)
        } else if account.is_empty() {
            Self::Deleted
        } else {
            Self::Updated(&account.info)
        }
    }
}
