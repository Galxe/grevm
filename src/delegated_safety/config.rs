/// Block-scoped runtime switches for the two EIP-7702 protections.
///
/// The switches are independent because delegated CREATE changes an EOA's nonce, while the
/// balance guard handles value movement that admission filtering cannot see without execution.
/// Both are opt-in and disabled by [`Default`], preserving upstream revm behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DelegatedSafetyConfig {
    /// Halts CREATE and CREATE2 in an EIP-7702 delegated account's execution context with
    /// `NotActivated`.
    ///
    /// For a top-level delegated call this is reported as a
    /// [`revm_context::result::ExecutionResult::Halt`].
    pub forbid_delegated_create: bool,
    /// Prevents delegated execution from consuming funds reserved for later transactions.
    ///
    /// A violation rolls back execution state and returns a charged top-level
    /// [`revm_context::result::ExecutionResult::Revert`]. The gas charge, transaction nonce, and
    /// EIP-7702 authorization effects remain applied.
    pub reserve_delegated_balance: bool,
}

impl DelegatedSafetyConfig {
    /// Disables both protections and preserves upstream revm semantics.
    pub const fn disabled() -> Self {
        Self { forbid_delegated_create: false, reserve_delegated_balance: false }
    }

    /// Enables only the delegated CREATE/CREATE2 guard.
    pub const fn create_only() -> Self {
        Self { forbid_delegated_create: true, reserve_delegated_balance: false }
    }

    /// Enables only delegated balance protection.
    pub const fn reserve_only() -> Self {
        Self { forbid_delegated_create: false, reserve_delegated_balance: true }
    }

    /// Enables both protections.
    pub const fn enabled() -> Self {
        Self { forbid_delegated_create: true, reserve_delegated_balance: true }
    }
}
