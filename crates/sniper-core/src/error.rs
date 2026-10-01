//! Error types for the sniper bot.

use thiserror::Error;

/// Main error type for the sniper bot.
#[derive(Error, Debug)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("Geyser connection error: {0}")]
    Geyser(String),

    #[error("Transaction decode error: {0}")]
    Decode(String),

    #[error("Transaction execution error: {0}")]
    Execution(String),

    #[error("Wallet error: {0}")]
    Wallet(String),

    #[error("Pool not found: {0}")]
    PoolNotFound(String),

    #[error("Insufficient balance: have {have} SOL, need {need} SOL")]
    InsufficientBalance { have: f64, need: f64 },

    #[error("Strategy rejected: {reason}")]
    StrategyRejected { reason: String },

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("Solana client error: {0}")]
    SolanaClient(String),

    #[error("Unknown error: {0}")]
    Unknown(String),
}

/// Result type alias using our Error type.
pub type Result<T> = std::result::Result<T, Error>;

impl From<solana_sdk::pubkey::ParsePubkeyError> for Error {
    fn from(e: solana_sdk::pubkey::ParsePubkeyError) -> Self {
        Error::Decode(format!("Invalid pubkey: {}", e))
    }
}

impl From<solana_sdk::signature::ParseSignatureError> for Error {
    fn from(e: solana_sdk::signature::ParseSignatureError) -> Self {
        Error::Decode(format!("Invalid signature: {}", e))
    }
}

impl From<bs58::decode::Error> for Error {
    fn from(e: bs58::decode::Error) -> Self {
        Error::Decode(format!("Base58 decode error: {}", e))
    }
}
