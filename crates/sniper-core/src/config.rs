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

    /// Position management (auto-sell) settings
    #[serde(default)]
    pub position: PositionConfig,

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

    /// Commitment level for the stream: processed, confirmed, finalized.
    /// `processed` is fastest and is what you want for sniping.
    #[serde(default = "default_geyser_commitment")]
    pub commitment: String,

    /// Connect timeout in milliseconds
    #[serde(default = "default_geyser_connect_timeout")]
    pub connect_timeout_ms: u64,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Tip paid to Jito validators per bundle, in lamports
    #[serde(default = "default_jito_tip")]
    pub jito_tip_lamports: u64,

    /// Optional Jito auth UUID (sent as `x-jito-auth`), for higher rate limits
    #[serde(default)]
    pub jito_auth_uuid: Option<String>,

    /// Also send via regular RPC when Jito is enabled (races both paths)
    #[serde(default)]
    pub jito_also_send_rpc: bool,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            jito_enabled: false,
            jito_endpoint: None,
            priority_fee_microlamports: default_priority_fee(),
            max_retries: default_retries(),
            compute_unit_limit: default_compute_limit(),
            simulate_first: true,
            jito_tip_lamports: default_jito_tip(),
            jito_auth_uuid: None,
            jito_also_send_rpc: false,
        }
    }
}

/// Position management configuration (auto-sell).
///
/// Exits are evaluated against the *sellable value* of the position
/// (what selling the whole bag into the curve would return right now),
/// not the spot price, so price impact and fees are already counted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PositionConfig {
    /// Enable automatic selling of opened positions
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Take profit when value reaches this % gain over cost (e.g. 100 = 2x)
    #[serde(default = "default_take_profit_pct")]
    pub take_profit_pct: f64,

    /// Stop loss when value drops this % below cost (e.g. 30 = -30%)
    #[serde(default = "default_stop_loss_pct")]
    pub stop_loss_pct: f64,

    /// Trailing stop: once in profit, sell if value falls this % from its peak.
    /// 0 disables.
    #[serde(default)]
    pub trailing_stop_pct: f64,

    /// Sell unconditionally after holding this many seconds. 0 disables.
    #[serde(default = "default_max_hold_secs")]
    pub max_hold_secs: u64,

    /// How often to re-price open positions, in milliseconds
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,

    /// Slippage tolerance for sells (0.0 - 1.0). Usually wider than buys.
    #[serde(default = "default_sell_slippage")]
    pub sell_slippage: f64,

    /// Retries for a failed sell before the position is marked failed
    #[serde(default = "default_retries")]
    pub max_sell_retries: u32,
}

impl Default for PositionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            take_profit_pct: default_take_profit_pct(),
            stop_loss_pct: default_stop_loss_pct(),
            trailing_stop_pct: 0.0,
            max_hold_secs: default_max_hold_secs(),
            poll_interval_ms: default_poll_interval_ms(),
            sell_slippage: default_sell_slippage(),
            max_sell_retries: default_retries(),
        }
    }
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

fn default_geyser_commitment() -> String {
    "processed".to_string()
}

fn default_geyser_connect_timeout() -> u64 {
    10_000
}

fn default_jito_tip() -> u64 {
    1_000_000 // 0.001 SOL
}

fn default_take_profit_pct() -> f64 {
    100.0
}

fn default_stop_loss_pct() -> f64 {
    30.0
}

fn default_max_hold_secs() -> u64 {
    300
}

fn default_poll_interval_ms() -> u64 {
    1_000
}

fn default_sell_slippage() -> f64 {
    0.25
}

/// Parse a commitment string; shared by the RPC and Geyser paths.
pub fn parse_commitment(s: &str) -> Result<solana_sdk::commitment_config::CommitmentLevel> {
    use solana_sdk::commitment_config::CommitmentLevel;
    match s.to_ascii_lowercase().as_str() {
        "processed" => Ok(CommitmentLevel::Processed),
        "confirmed" => Ok(CommitmentLevel::Confirmed),
        "finalized" => Ok(CommitmentLevel::Finalized),
        other => Err(Error::Config(format!("unknown commitment level '{}'", other))),
    }
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

        parse_commitment(&self.rpc.commitment)?;

        if let Some(geyser) = &self.geyser {
            if geyser.endpoint.is_empty() {
                return Err(Error::Config("geyser.endpoint is empty".to_string()));
            }
            parse_commitment(&geyser.commitment)?;
        }

        if self.execution.jito_enabled && self.execution.jito_tip_lamports < 1_000 {
            return Err(Error::Config(
                "jito_tip_lamports must be at least 1000 (Jito minimum)".to_string(),
            ));
        }

        let p = &self.position;
        if p.enabled {
            if p.take_profit_pct <= 0.0 {
                return Err(Error::Config("position.take_profit_pct must be > 0".to_string()));
            }
            if !(0.0..100.0).contains(&p.stop_loss_pct) || p.stop_loss_pct == 0.0 {
                return Err(Error::Config(
                    "position.stop_loss_pct must be in (0, 100)".to_string(),
                ));
            }
            if !(0.0..100.0).contains(&p.trailing_stop_pct) {
                return Err(Error::Config(
                    "position.trailing_stop_pct must be in [0, 100)".to_string(),
                ));
            }
            if !(0.0..=1.0).contains(&p.sell_slippage) {
                return Err(Error::Config(
                    "position.sell_slippage must be between 0.0 and 1.0".to_string(),
                ));
            }
            if p.poll_interval_ms == 0 {
                return Err(Error::Config("position.poll_interval_ms must be > 0".to_string()));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Config {
        Config {
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
            position: PositionConfig::default(),
            logging: LoggingConfig::default(),
        }
    }

    #[test]
    fn test_config_validation() {
        assert!(base_config().validate().is_ok());
    }

    #[test]
    fn rejects_bad_position_config() {
        let mut c = base_config();
        c.position.stop_loss_pct = 120.0;
        assert!(c.validate().is_err());

        let mut c = base_config();
        c.position.take_profit_pct = 0.0;
        assert!(c.validate().is_err());

        // Disabled position management skips those checks.
        let mut c = base_config();
        c.position.enabled = false;
        c.position.stop_loss_pct = 120.0;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_tiny_jito_tip() {
        let mut c = base_config();
        c.execution.jito_enabled = true;
        c.execution.jito_tip_lamports = 10;
        assert!(c.validate().is_err());
    }

    #[test]
    fn example_config_parses() {
        let raw = include_str!("../../../config.example.toml");
        let c: Config = toml::from_str(raw).expect("config.example.toml must parse");
        c.validate().expect("config.example.toml must validate");
    }

    #[test]
    fn minimal_config_gets_defaults() {
        let raw = r#"
            [rpc]
            endpoint = "http://localhost:8899"
            [wallet]
            keypair_path = "k.json"
            [strategy]
            [geyser]
            endpoint = "http://localhost:10000"
        "#;
        let c: Config = toml::from_str(raw).unwrap();
        c.validate().unwrap();
        assert_eq!(c.geyser.unwrap().commitment, "processed");
        assert_eq!(c.execution.jito_tip_lamports, 1_000_000);
        assert_eq!(c.position.take_profit_pct, 100.0);
    }
}
