//! # Sniper Core
//!
//! Core types, configuration, and error handling for the Solana sniper bot.
//! This crate provides the foundational building blocks used across all other crates.

pub mod config;
pub mod error;
pub mod types;

pub use config::Config;
pub use error::{Error, Result};
pub use types::*;
