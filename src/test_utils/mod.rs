//! This module contains utilities for testing and benchmarking the Grevm library.

pub mod common;
pub mod erc20;
pub mod uniswap;

/// Gas limit for native transfer transactions.
pub const TRANSFER_GAS_LIMIT: u64 = 21_000;
