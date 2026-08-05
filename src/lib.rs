//! # Grevm
//!
//! Grevm is a high-performance, parallelized Ethereum Virtual Machine (EVM) inspired by BlockSTM
//! designed to handle concurrent transaction execution and validation. It provides utilities for
//! managing transaction states, dependencies, and memory, while leveraging multi-threading to
//! maximize throughput.
//!
//! ## Concurrency
//!
//! By default, Grevm creates one speculative worker per logical CPU reported by
//! [`std::thread::available_parallelism`] (falling back to eight if it is unavailable).
//! Integrations can override the worker count through [`GrevmConfig::concurrency_level`].
//!
//! ## Error Handling
//!
//! Errors during execution are encapsulated in the `GrevmError` type, which includes the
//! transaction ID and the underlying EVM error. This allows for precise debugging and error
//! reporting.
mod account;
mod beneficiary;
mod bundle;
mod config;
mod delegated_safety;
mod incarnation_db;
mod model;
mod outcome;
mod parallel_state;
mod precompile;
mod scheduler;
#[cfg(feature = "test-utils")]
pub mod test_utils;
mod tx_dependency;

pub(crate) use model::{
    AbortReason, AccountBasic, LocationAndType, MVMemory, MemoryEntry, MemoryValue, ReadVersion,
    Task, TransactionResult, TransactionStatus, TxId, TxState, TxVersion,
};

pub use bundle::{ParallelBundleState, ParallelTakeBundle};
pub use config::GrevmConfig;
pub use delegated_safety::DelegatedSafetyConfig;
pub use outcome::{GrevmError, TxExecutionOutcome};
pub use parallel_state::{ParallelCacheState, ParallelState};
pub use precompile::{
    DynParallelPrecompile, ParallelPrecompile, ParallelPrecompileError, ParallelPrecompileInput,
    ParallelPrecompileResult, ParallelPrecompileState,
};
pub use revm_context::result::InvalidTransaction;
pub use scheduler::Scheduler;
