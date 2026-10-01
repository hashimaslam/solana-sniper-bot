//! Configuration structures for the sniper bot.

use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::{Error, Result};

/// Root configuration structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// RPC connection settings
    pub rpc: RpcConfig,

    /// Geyser gRPC settings (optional, for lower latency)
    #[serde(default)]
    pub geyser: Option<GeyserConfig>,

    /// Wallet configuration
    pub wallet: WalletConfig,

    /// Trading strategy settings
    pub strategy: StrategyConfig,

    /// Execution settings
    #[serde(default)]
    pub execution: ExecutionConfig,

    /// Logging settings
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// RPC connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcConfig {
    /// HTTP RPC endpoint URL
    pub endpoint: String,

    /// WebSocket endpoint URL (defaults to ws version of endpoint)
    pub ws_endpoint: Option<String>,

    /// Commitment level for queries
    #[serde(default = "default_commitment")]
    pub commitment: String,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Geyser gRPC configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeyserConfig {
    /// Geyser gRPC endpoint
    pub endpoint: String,

    /// Authentication token (if required)
    pub token: Option<String>,

    /// Enable TLS
    #[serde(default = "default_true")]
    pub tls: bool,
}

/// Wallet configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
    /// Path to keypair JSON file
    pub keypair_path: String,

    /// Additional wallets for parallel execution
    #[serde(default)]
    pub additional_keypairs: Vec<String>,
}

/// Trading strategy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    /// Maximum SOL to spend per snipe
    #[serde(default = "default_max_buy")]
    pub max_buy_sol: f64,

    /// Minimum pool liquidity in SOL to trigger snipe
    #[serde(default = "default_min_liquidity")]
    pub min_liquidity_sol: f64,

    /// Maximum pool age in seconds to consider
    #[serde(default = "default_max_age")]
    pub max_pool_age_secs: u64,

    /// Slippage tolerance (0.0 - 1.0)
    #[serde(default = "default_slippage")]
    pub slippage: f64,

    /// Enable pump.fun pool detection
    #[serde(default = "default_true")]
    pub pump_fun_enabled: bool,

    /// Enable Raydium pool detection
    #[serde(default)]
    pub raydium_enabled: bool,

    /// Blacklisted token mints (known rugs)
    #[serde(default)]
    pub blacklist: Vec<String>,

    /// Only snipe tokens from whitelisted creators
    #[serde(default)]
    pub creator_whitelist: Vec<String>,
}

/// Execution configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExecutionConfig {
    /// Use Jito bundles for MEV protection
    #[serde(default)]
    pub jito_enabled: bool,

    /// Jito block engine endpoint
    pub jito_endpoint: Option<String>,

    /// Priority fee in microlamports
    #[serde(default = "default_priority_fee")]
    pub priority_fee_microlamports: u64,

    /// Number of retry attempts
    #[serde(default = "default_retries")]
    pub max_retries: u32,

    /// Compute unit limit
    #[serde(default = "default_compute_limit")]
    pub compute_unit_limit: u32,

    /// Simulate transaction before sending
    #[serde(default = "default_true")]
    pub simulate_first: bool,
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error)
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Log to file
    pub file: Option<String>,

    /// Enable JSON formatted logs
    #[serde(default)]
    pub json: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            file: None,
            json: false,
        }
    }
}

// Default value functions
fn default_commitment() -> String {
    "confirmed".to_string()
}

fn default_timeout_ms() -> u64 {
    30000
}

fn default_true() -> bool {
    true
}

fn default_max_buy() -> f64 {
    0.1
}

fn default_min_liquidity() -> f64 {
    1.0
}

fn default_max_age() -> u64 {
    60
}

fn default_slippage() -> f64 {
    0.15
}

fn default_priority_fee() -> u64 {
    100_000
}

fn default_retries() -> u32 {
    3
}

fn default_compute_limit() -> u32 {
    200_000
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            Error::Config(format!(
                "Failed to read config file '{}': {}",
                path.as_ref().display(),
                e
            ))
        })?;

        let config: Config = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Validate configuration values.
    pub fn validate(&self) -> Result<()> {
        if self.strategy.max_buy_sol <= 0.0 {
            return Err(Error::Config("max_buy_sol must be positive".to_string()));
        }

        if self.strategy.slippage < 0.0 || self.strategy.slippage > 1.0 {
            return Err(Error::Config(
                "slippage must be between 0.0 and 1.0".to_string(),
            ));
        }

        if self.rpc.endpoint.is_empty() {
            return Err(Error::Config("RPC endpoint is required".to_string()));
        }

        if self.wallet.keypair_path.is_empty() {
            return Err(Error::Config("Wallet keypair path is required".to_string()));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_validation() {
        let config = Config {
            rpc: RpcConfig {
                endpoint: "https://api.mainnet-beta.solana.com".to_string(),
                ws_endpoint: None,
                commitment: "confirmed".to_string(),
                timeout_ms: 30000,
            },
            geyser: None,
            wallet: WalletConfig {
                keypair_path: "/path/to/keypair.json".to_string(),
                additional_keypairs: vec![],
            },
            strategy: StrategyConfig {
                max_buy_sol: 0.1,
                min_liquidity_sol: 1.0,
                max_pool_age_secs: 60,
                slippage: 0.15,
                pump_fun_enabled: true,
                raydium_enabled: false,
                blacklist: vec![],
                creator_whitelist: vec![],
            },
            execution: ExecutionConfig::default(),
            logging: LoggingConfig::default(),
        };

        assert!(config.validate().is_ok());
    }
}
