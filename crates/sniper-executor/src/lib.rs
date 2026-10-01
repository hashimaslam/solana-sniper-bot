//! # Sniper Executor
//!
//! Transaction building, signing, and submission for sniping pools.
//! Supports priority fees, compute limits, and Jito bundles.

pub mod pump_fun;
pub mod transaction;

pub use pump_fun::PumpFunExecutor;
pub use transaction::TransactionBuilder;
