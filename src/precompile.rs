//! Capability-restricted custom precompiles for parallel execution.
//!
//! The adapter in this module is the only layer that receives Alloy's unrestricted
//! [`PrecompileInput`]. Implementations receive [`ParallelPrecompileInput`] instead, so EVM state
//! can only be accessed through the journal-aware [`ParallelPrecompileState`] facade.

use alloy_evm::{
    EvmInternals, EvmInternalsError,
    precompiles::{DynPrecompile, PrecompileInput},
};
use revm::{
    interpreter::{SStoreResult, StateLoad},
    precompile::{PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput},
};
use revm_primitives::{Address, U256};
use std::{error::Error, fmt, sync::Arc};

/// A custom precompile whose EVM state access is safe for speculative parallel execution.
///
/// Implementations may only access EVM state through [`ParallelPrecompileInput::state`]. They must
/// still be safe to invoke concurrently and more than once for the same transaction: speculative
/// attempts can be discarded and retried, while clones share the same implementation. In
/// particular, implementations must not retain consensus-visible mutable state or perform
/// out-of-band state mutations.
pub trait ParallelPrecompile: Send + Sync + 'static {
    /// Returns this precompile's identifier.
    fn precompile_id(&self) -> &PrecompileId;

    /// Executes the precompile with capability-restricted input.
    ///
    /// The implementation remains responsible for checking the forwarded gas and constructing a
    /// complete [`PrecompileOutput`], including ordinary gas and state-gas accounting.
    fn call(&self, input: &mut ParallelPrecompileInput<'_>) -> ParallelPrecompileResult;
}

/// A cloneable, dynamically dispatched [`ParallelPrecompile`].
#[derive(Clone)]
pub struct DynParallelPrecompile(Arc<dyn ParallelPrecompile>);

impl DynParallelPrecompile {
    /// Wraps a concrete restricted precompile for dynamic registration.
    pub fn from_precompile<P>(precompile: P) -> Self
    where
        P: ParallelPrecompile,
    {
        Self(Arc::new(precompile))
    }

    /// Creates a restricted precompile from a closure.
    pub fn new<F>(id: PrecompileId, f: F) -> Self
    where
        F: for<'a> Fn(&mut ParallelPrecompileInput<'a>) -> ParallelPrecompileResult
            + Send
            + Sync
            + 'static,
    {
        Self(Arc::new(ClosurePrecompile { id, f }))
    }

    /// Returns an Alloy precompile that can only forward restricted input to the implementation.
    ///
    /// The adapter always disables Alloy's input-only result cache. Restricted precompiles may
    /// depend on journal state or immutable block-scoped providers, neither of which is represented
    /// by Alloy's cache key.
    pub fn to_alloy(&self) -> DynPrecompile {
        let id = self.precompile_id().clone();
        let precompile = self.clone();
        DynPrecompile::new_stateful(id, move |input| {
            let reservoir = input.reservoir;
            let mut input = ParallelPrecompileInput::from_alloy(input);
            let result = precompile.call(&mut input);
            let result = input.state.take_fault().map_or(result, Err);
            match result {
                Ok(output) => Ok(output),
                Err(ParallelPrecompileError::Halt(reason)) => {
                    Ok(PrecompileOutput::halt(reason, reservoir))
                }
                Err(ParallelPrecompileError::Fatal(error)) => Err(error),
            }
        })
    }
}

impl ParallelPrecompile for DynParallelPrecompile {
    fn precompile_id(&self) -> &PrecompileId {
        self.0.precompile_id()
    }

    fn call(&self, input: &mut ParallelPrecompileInput<'_>) -> ParallelPrecompileResult {
        self.0.call(input)
    }
}

impl fmt::Debug for DynParallelPrecompile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DynParallelPrecompile")
            .field("id", self.precompile_id())
            .finish_non_exhaustive()
    }
}

struct ClosurePrecompile<F> {
    id: PrecompileId,
    f: F,
}

impl<F> ParallelPrecompile for ClosurePrecompile<F>
where
    F: for<'a> Fn(&mut ParallelPrecompileInput<'a>) -> ParallelPrecompileResult
        + Send
        + Sync
        + 'static,
{
    fn precompile_id(&self) -> &PrecompileId {
        &self.id
    }

    fn call(&self, input: &mut ParallelPrecompileInput<'_>) -> ParallelPrecompileResult {
        (self.f)(input)
    }
}

/// Capability-restricted input for a parallel custom precompile.
///
/// All fields are private so an implementation cannot recover Alloy's [`PrecompileInput`] or its
/// unrestricted [`EvmInternals`].
///
/// ```compile_fail
/// fn raw_database(mut input: grevm::ParallelPrecompileInput<'_>) {
///     input.internals_mut().db_mut();
/// }
/// ```
pub struct ParallelPrecompileInput<'a> {
    data: &'a [u8],
    gas: u64,
    reservoir: u64,
    caller: Address,
    value: U256,
    target_address: Address,
    bytecode_address: Address,
    is_static: bool,
    state: ParallelPrecompileState<'a>,
}

impl<'a> ParallelPrecompileInput<'a> {
    fn from_alloy(input: PrecompileInput<'a>) -> Self {
        let PrecompileInput {
            data,
            gas,
            reservoir,
            caller,
            value,
            target_address,
            is_static,
            bytecode_address,
            internals,
        } = input;
        Self {
            data,
            gas,
            reservoir,
            caller,
            value,
            target_address,
            bytecode_address,
            is_static,
            state: ParallelPrecompileState { internals, is_static, fault: None },
        }
    }

    /// Returns the call data.
    pub const fn data(&self) -> &[u8] {
        self.data
    }

    /// Returns the gas forwarded to this call.
    pub const fn gas(&self) -> u64 {
        self.gas
    }

    /// Returns the EIP-8037 state-gas reservoir. This is zero on mainnet.
    pub const fn reservoir(&self) -> u64 {
        self.reservoir
    }

    /// Returns the immediate caller.
    pub const fn caller(&self) -> Address {
        self.caller
    }

    /// Returns the value attached to this call.
    pub const fn value(&self) -> U256 {
        self.value
    }

    /// Returns the call's execution and storage-context address.
    pub const fn target_address(&self) -> Address {
        self.target_address
    }

    /// Returns the address whose bytecode selected this precompile.
    pub const fn bytecode_address(&self) -> Address {
        self.bytecode_address
    }

    /// Returns whether the target and bytecode addresses are identical.
    pub fn is_direct_call(&self) -> bool {
        self.target_address == self.bytecode_address
    }

    /// Returns whether the call is executing in a static context.
    pub const fn is_static(&self) -> bool {
        self.is_static
    }

    /// Returns the journal-aware state facade.
    pub const fn state(&mut self) -> &mut ParallelPrecompileState<'a> {
        &mut self.state
    }
}

impl fmt::Debug for ParallelPrecompileInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParallelPrecompileInput")
            .field("data", &self.data)
            .field("gas", &self.gas)
            .field("reservoir", &self.reservoir)
            .field("caller", &self.caller)
            .field("value", &self.value)
            .field("target_address", &self.target_address)
            .field("bytecode_address", &self.bytecode_address)
            .field("is_static", &self.is_static)
            .finish_non_exhaustive()
    }
}

/// Journal-aware EVM state operations available to a parallel custom precompile.
///
/// This facade intentionally exposes owned values only. It does not provide access to the raw
/// database, mutable journal accounts, code mutation, checkpoints, logs, or transient storage.
/// These operations report warm/cold metadata but do not calculate or deduct gas or refunds.
/// Storage operations preserve revm's normal account-first journal semantics: accessing a slot may
/// load its account before resolving the storage value. Consequently, accessing beneficiary
/// storage may also participate in beneficiary-history coordination. The lower-level database
/// still tracks beneficiary storage independently as a correctness fallback, but this facade never
/// bypasses the journal's account lifecycle.
pub struct ParallelPrecompileState<'a> {
    internals: EvmInternals<'a>,
    is_static: bool,
    fault: Option<ParallelPrecompileError>,
}

impl ParallelPrecompileState<'_> {
    /// Loads an account balance through the journal.
    pub fn balance(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<U256>, ParallelPrecompileError> {
        self.ensure_healthy()?;
        match self.internals.load_account(address) {
            Ok(load) => Ok(load.map(|account| account.info.balance)),
            Err(error) => self.record_fault(ParallelPrecompileError::database(error)),
        }
    }

    /// Loads a storage value through the journal, including its normal account load.
    pub fn sload(
        &mut self,
        address: Address,
        key: U256,
    ) -> Result<StateLoad<U256>, ParallelPrecompileError> {
        self.ensure_healthy()?;
        match self.internals.sload(address, key) {
            Ok(load) => Ok(load),
            Err(error) => self.record_fault(ParallelPrecompileError::database(error)),
        }
    }

    /// Sets an account balance through the journal and marks the account as touched.
    ///
    /// The returned load metadata tells the implementation whether the account access was cold,
    /// without exposing the mutable journal account itself.
    pub fn set_balance(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<StateLoad<()>, ParallelPrecompileError> {
        self.ensure_mutable()?;
        let error = match self.internals.load_account_mut(address) {
            Ok(load) => return Ok(load.map(|mut account| account.set_balance(balance))),
            Err(error) => error,
        };
        self.record_fault(ParallelPrecompileError::database(error))
    }

    /// Stores a value through the journal.
    pub fn sstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<StateLoad<SStoreResult>, ParallelPrecompileError> {
        self.ensure_mutable()?;
        match self.internals.sstore(address, key, value) {
            Ok(load) => Ok(load),
            Err(error) => self.record_fault(ParallelPrecompileError::database(error)),
        }
    }

    fn ensure_healthy(&self) -> Result<(), ParallelPrecompileError> {
        self.fault.clone().map_or(Ok(()), Err)
    }

    fn ensure_mutable(&mut self) -> Result<(), ParallelPrecompileError> {
        self.ensure_healthy()?;
        if self.is_static {
            self.record_fault(ParallelPrecompileError::Halt(PrecompileHalt::other_static(
                "state change during static call",
            )))
        } else {
            Ok(())
        }
    }

    fn record_fault<T>(
        &mut self,
        fault: ParallelPrecompileError,
    ) -> Result<T, ParallelPrecompileError> {
        let fault = self.fault.get_or_insert(fault).clone();
        Err(fault)
    }

    fn take_fault(&mut self) -> Option<ParallelPrecompileError> {
        self.fault.take()
    }
}

impl fmt::Debug for ParallelPrecompileState<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParallelPrecompileState")
            .field("is_static", &self.is_static)
            .finish_non_exhaustive()
    }
}

/// Result returned by a capability-restricted parallel precompile.
pub type ParallelPrecompileResult = Result<PrecompileOutput, ParallelPrecompileError>;

/// A non-fatal halt or fatal error produced by a parallel precompile.
#[derive(Clone, Debug)]
pub enum ParallelPrecompileError {
    /// A normal EVM halt that fails the current call without aborting EVM execution.
    Halt(PrecompileHalt),
    /// An unrecoverable error that aborts EVM execution.
    Fatal(PrecompileError),
}

impl ParallelPrecompileError {
    fn database(error: EvmInternalsError) -> Self {
        Self::Fatal(PrecompileError::Fatal(error.to_string()))
    }
}

impl From<PrecompileError> for ParallelPrecompileError {
    fn from(error: PrecompileError) -> Self {
        Self::Fatal(error)
    }
}

impl From<PrecompileHalt> for ParallelPrecompileError {
    fn from(reason: PrecompileHalt) -> Self {
        Self::Halt(reason)
    }
}

impl fmt::Display for ParallelPrecompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Halt(reason) => reason.fmt(f),
            Self::Fatal(error) => error.fmt(f),
        }
    }
}

impl Error for ParallelPrecompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Halt(reason) => Some(reason),
            Self::Fatal(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_evm::{eth::EthEvmContext, precompiles::Precompile as AlloyPrecompile};
    use revm::{
        Database,
        database::EmptyDB,
        precompile::{PrecompileResult, PrecompileStatus},
    };
    use revm_context::{DBErrorMarker, JournalTr};
    use revm_primitives::{B256, Bytes};
    use revm_state::{AccountInfo, Bytecode};

    const ADDRESS: Address = Address::with_last_byte(0x42);
    const PRECOMPILE: Address = Address::with_last_byte(0xfe);

    fn call<DB>(
        ctx: &mut EthEvmContext<DB>,
        precompile: &DynParallelPrecompile,
        is_static: bool,
    ) -> PrecompileResult
    where
        DB: Database + fmt::Debug,
    {
        AlloyPrecompile::call(
            &precompile.to_alloy(),
            PrecompileInput {
                data: b"input",
                gas: 100_000,
                reservoir: 7,
                caller: Address::with_last_byte(1),
                value: U256::from(2),
                target_address: PRECOMPILE,
                is_static,
                bytecode_address: PRECOMPILE,
                internals: EvmInternals::from_context(ctx),
            },
        )
    }

    fn word(value: U256) -> Bytes {
        Bytes::copy_from_slice(&value.to_be_bytes::<32>())
    }

    struct ConcretePrecompile(PrecompileId);

    impl ParallelPrecompile for ConcretePrecompile {
        fn precompile_id(&self) -> &PrecompileId {
            &self.0
        }

        fn call(&self, input: &mut ParallelPrecompileInput<'_>) -> ParallelPrecompileResult {
            Ok(PrecompileOutput::new(1, Bytes::from_static(b"concrete"), input.reservoir()))
        }
    }

    #[derive(Debug)]
    struct FailingDbError;

    impl fmt::Display for FailingDbError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("injected database failure")
        }
    }

    impl Error for FailingDbError {}
    impl DBErrorMarker for FailingDbError {}

    #[derive(Debug)]
    struct FailingDb;

    impl Database for FailingDb {
        type Error = FailingDbError;

        fn basic(&mut self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(FailingDbError)
        }

        fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Err(FailingDbError)
        }

        fn storage(&mut self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Err(FailingDbError)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Err(FailingDbError)
        }
    }

    #[test]
    fn adapter_forwards_metadata_and_disables_caching() {
        let precompile = DynParallelPrecompile::new(PrecompileId::custom("metadata"), |input| {
            assert_eq!(input.data(), b"input");
            assert_eq!(input.gas(), 100_000);
            assert_eq!(input.reservoir(), 7);
            assert_eq!(input.caller(), Address::with_last_byte(1));
            assert_eq!(input.value(), U256::from(2));
            assert_eq!(input.target_address(), PRECOMPILE);
            assert_eq!(input.bytecode_address(), PRECOMPILE);
            assert!(input.is_direct_call());
            assert!(!input.is_static());
            Ok(PrecompileOutput::new(3, Bytes::new(), input.reservoir()))
        });
        let alloy = precompile.to_alloy();
        assert!(!AlloyPrecompile::supports_caching(&alloy));

        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());
        let output = call(&mut ctx, &precompile, false).unwrap();
        assert_eq!(output.gas_used, 3);
        assert_eq!(output.reservoir, 7);
    }

    #[test]
    fn concrete_trait_implementation_can_be_registered() {
        let precompile = DynParallelPrecompile::from_precompile(ConcretePrecompile(
            PrecompileId::custom("concrete"),
        ));
        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());
        let output = call(&mut ctx, &precompile, false).unwrap();
        assert_eq!(output.gas_used, 1);
        assert_eq!(output.bytes, Bytes::from_static(b"concrete"));
    }

    #[test]
    fn state_read_observes_an_earlier_journal_write() {
        let key = U256::from(3);
        let value = U256::from(9);
        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());
        ctx.journaled_state.load_account(ADDRESS).unwrap();
        ctx.journaled_state.sstore(ADDRESS, key, value).unwrap();

        let precompile = DynParallelPrecompile::new(PrecompileId::custom("sload"), move |input| {
            let loaded = input.state().sload(ADDRESS, key)?;
            Ok(PrecompileOutput::new(0, word(loaded.data), input.reservoir()))
        });
        let output = call(&mut ctx, &precompile, false).unwrap();
        assert_eq!(output.bytes, word(value));
    }

    #[test]
    fn ignored_static_mutation_is_forced_to_halt_without_writing() {
        let key = U256::from(4);
        let precompile =
            DynParallelPrecompile::new(PrecompileId::custom("static-write"), move |input| {
                // The adapter must enforce the recorded fault even if an implementation
                // accidentally ignores the returned error and reports success.
                let _ = input.state().sstore(ADDRESS, key, U256::from(11));
                Ok(PrecompileOutput::new(0, Bytes::new(), input.reservoir()))
            });
        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());
        ctx.journaled_state.load_account(ADDRESS).unwrap();
        let output = call(&mut ctx, &precompile, true).unwrap();
        assert!(matches!(output.status, PrecompileStatus::Halt(_)));
        assert_eq!(ctx.journaled_state.sload(ADDRESS, key).unwrap().data, U256::ZERO);
    }

    #[test]
    fn state_write_follows_enclosing_journal_checkpoint_revert() {
        let key = U256::from(5);
        let value = U256::from(13);
        let precompile = DynParallelPrecompile::new(PrecompileId::custom("sstore"), move |input| {
            input.state().sstore(ADDRESS, key, value)?;
            Ok(PrecompileOutput::new(0, Bytes::new(), input.reservoir()))
        });
        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());
        let checkpoint = ctx.journaled_state.checkpoint();

        assert!(call(&mut ctx, &precompile, false).unwrap().is_success());
        assert_eq!(ctx.journaled_state.sload(ADDRESS, key).unwrap().data, value);
        ctx.journaled_state.checkpoint_revert(checkpoint);
        assert_eq!(ctx.journaled_state.sload(ADDRESS, key).unwrap().data, U256::ZERO);
    }

    #[test]
    fn set_balance_reports_cold_then_warm_without_exposing_the_account() {
        let precompile = DynParallelPrecompile::new(
            PrecompileId::custom("set-balance-access-status"),
            move |input| {
                let first = input.state().set_balance(ADDRESS, U256::from(1))?;
                let second = input.state().set_balance(ADDRESS, U256::from(2))?;
                Ok(PrecompileOutput::new(
                    0,
                    Bytes::from(vec![u8::from(first.is_cold), u8::from(second.is_cold)]),
                    input.reservoir(),
                ))
            },
        );
        let mut ctx = EthEvmContext::new(EmptyDB::default(), Default::default());
        let output = call(&mut ctx, &precompile, false).unwrap();
        assert_eq!(output.bytes, Bytes::from_static(&[1, 0]));
        assert_eq!(ctx.journaled_state.load_account(ADDRESS).unwrap().info.balance, U256::from(2));
    }

    #[test]
    fn ignored_database_fault_is_forced_to_be_fatal() {
        let precompile = DynParallelPrecompile::new(
            PrecompileId::custom("ignored-database-error"),
            move |input| {
                let _ = input.state().balance(ADDRESS);
                Ok(PrecompileOutput::new(0, Bytes::new(), input.reservoir()))
            },
        );
        let mut ctx = EthEvmContext::new(FailingDb, Default::default());
        assert!(matches!(
            call(&mut ctx, &precompile, false),
            Err(PrecompileError::Fatal(message)) if message.contains("injected database failure")
        ));
    }
}
