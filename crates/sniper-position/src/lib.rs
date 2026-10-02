//! # Sniper Position
//!
//! Tracks positions opened by the executor and closes them automatically on
//! take-profit, stop-loss, trailing stop, or max hold time. Positions are
//! valued at what selling the whole bag would return right now, so fees and
//! price impact are part of every decision.

pub mod manager;
pub mod rules;

pub use manager::{Position, PositionEvent, PositionManager, PositionStatus};
pub use rules::ExitReason;
