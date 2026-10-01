//! # Sniper Decoder
//!
//! Decodes SPL Token and pump.fun instructions to detect pool creation events.

pub mod pump_fun;
pub mod traits;

pub use pump_fun::PumpFunDecoder;
pub use traits::{Decoder, DecodedInstruction};
