//! Grevm-local EIP-7702 delegated execution safety.
//!
//! This module intentionally builds on upstream revm extension points instead of forking revm:
//! custom instructions block CREATE/CREATE2 in delegated execution contexts, and a wrapping
//! journal plus handler enforce reserve-balance semantics around the top-level execution frame.

mod handler;
mod instructions;
mod journal;
mod policy;
mod precompiles;

pub(crate) use handler::{GravityHandler, RewardMode};
pub(crate) use instructions::gravity_instructions;
pub(crate) use journal::{TrackingJournal, TrackingJournalExt};
pub use policy::DelegatedSafetyConfig;
pub(crate) use policy::{ReservePlan, ReservePlanError, SharedReservePlan};
pub(crate) use precompiles::TrackingPrecompilesMap;
