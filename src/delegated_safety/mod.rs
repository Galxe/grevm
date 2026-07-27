//! Grevm-local EIP-7702 delegated execution safety.
//!
//! This module intentionally builds on upstream revm extension points instead of forking revm:
//! custom instructions block CREATE/CREATE2 in delegated execution contexts, and a transaction
//! handler prevents delegated execution from consuming the conservative cost of later sender
//! transactions.

mod config;
mod handler;
mod instructions;
mod reserve;

pub use config::DelegatedSafetyConfig;
pub(crate) use handler::{BeneficiaryMode, GrevmHandler, ReserveMode};
pub(crate) use instructions::gravity_instructions;
pub(crate) use reserve::{ReserveJournalExt, ReservePlanner};
