//! # Grevm
//!
//! Grevm is a high-performance, parallelized Ethereum Virtual Machine (EVM) inspired by BlockSTM
//! designed to handle concurrent transaction execution and validation. It provides utilities for
//! managing transaction states, dependencies, and memory, while leveraging multi-threading to
//! maximize throughput.
//!
//! ## Concurrency
//!
//! Grevm automatically determines the optimal level of concurrency based on the available CPU
//! cores. Integrations can override it through [`GrevmConfig::concurrency_level`].
//!
//! ## Error Handling
//!
//! Errors during execution are encapsulated in the `GrevmError` type, which includes the
//! transaction ID and the underlying EVM error. This allows for precise debugging and error
//! reporting.
mod async_commit;
mod bundle;
mod cache_db;
mod config;
mod delegated_safety;
mod hint;
mod model;
mod outcome;
mod parallel_state;
mod scheduler;
#[cfg(feature = "test-utils")]
pub mod test_utils;
mod tx_dependency;
mod utils;

pub(crate) use model::{
    AbortReason, AccountBasic, LocationAndType, MVMemory, MemoryEntry, MemoryValue, ReadVersion,
    Task, TransactionResult, TransactionStatus, TxId, TxState, TxVersion,
};

pub use bundle::{ParallelBundleState, ParallelTakeBundle};
pub use config::GrevmConfig;
pub use delegated_safety::DelegatedSafetyConfig;
pub use outcome::{GrevmError, SkipReason, TxExecutionOutcome};
pub use parallel_state::{ParallelCacheState, ParallelState};
pub use scheduler::Scheduler;
pub use utils::fork_join_util;
