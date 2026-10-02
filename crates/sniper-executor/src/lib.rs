//! # Sniper Executor
//!
//! Transaction building, signing, and submission for sniping pools.
//! Supports priority fees, compute limits, and pluggable senders (RPC, Jito).

pub mod pump_fun;
pub mod sender;
pub mod transaction;

pub use pump_fun::PumpFunExecutor;
pub use sender::{RaceSender, RpcSender, TxSender};
pub use transaction::{Executor, TransactionBuilder};
