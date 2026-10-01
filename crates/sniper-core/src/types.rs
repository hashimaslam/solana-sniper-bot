//! Core types used across the sniper bot.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::fmt;

/// Represents a detected liquidity pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pool {
    /// Pool address
    pub address: Pubkey,

    /// Token mint address (the token being traded)
    pub token_mint: Pubkey,

    /// Quote mint (usually SOL or WSOL)
    pub quote_mint: Pubkey,

    /// Pool type
    pub pool_type: PoolType,

    /// Initial liquidity in lamports
    pub initial_liquidity_lamports: u64,

    /// Token reserve amount
    pub token_reserve: u64,

    /// Creation slot
    pub creation_slot: u64,

    /// Creation timestamp
    pub created_at: DateTime<Utc>,

    /// Creator/deployer address
    pub creator: Option<Pubkey>,

    /// Associated bonding curve (for pump.fun)
    pub bonding_curve: Option<Pubkey>,
}

impl Pool {
    /// Get initial liquidity in SOL.
    pub fn liquidity_sol(&self) -> f64 {
        self.initial_liquidity_lamports as f64 / 1_000_000_000.0
    }

    /// Get pool age in seconds.
    pub fn age_secs(&self) -> i64 {
        (Utc::now() - self.created_at).num_seconds()
    }
}

/// Type of liquidity pool/AMM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolType {
    /// pump.fun bonding curve
    PumpFun,
    /// Raydium AMM V4
    RaydiumV4,
    /// Raydium CPMM (Concentrated)
    RaydiumCpmm,
    /// Orca Whirlpool
    Orca,
    /// Meteora DLMM
    Meteora,
    /// Unknown/Other
    Unknown,
}

impl fmt::Display for PoolType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PoolType::PumpFun => write!(f, "pump.fun"),
            PoolType::RaydiumV4 => write!(f, "Raydium V4"),
            PoolType::RaydiumCpmm => write!(f, "Raydium CPMM"),
            PoolType::Orca => write!(f, "Orca"),
            PoolType::Meteora => write!(f, "Meteora"),
            PoolType::Unknown => write!(f, "Unknown"),
        }
    }
}

/// A parsed transaction from the listener.
#[derive(Debug, Clone)]
pub struct ParsedTransaction {
    /// Transaction signature
    pub signature: Signature,

    /// Slot number
    pub slot: u64,

    /// Block time (if available)
    pub block_time: Option<DateTime<Utc>>,

    /// Raw transaction data
    pub data: Vec<u8>,

    /// Account keys involved
    pub account_keys: Vec<Pubkey>,

    /// Program IDs invoked
    pub program_ids: Vec<Pubkey>,

    /// Was the transaction successful
    pub success: bool,
}

/// Decoded pool creation event.
#[derive(Debug, Clone)]
pub struct PoolCreationEvent {
    /// The created pool
    pub pool: Pool,

    /// Transaction that created the pool
    pub signature: Signature,

    /// Slot of creation
    pub slot: u64,

    /// Raw instruction data
    pub instruction_data: Vec<u8>,
}

/// Result of strategy analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisResult {
    /// Should we snipe this pool?
    pub should_snipe: bool,

    /// Confidence score (0.0 - 1.0)
    pub confidence: f64,

    /// Recommended buy amount in SOL
    pub recommended_amount_sol: f64,

    /// Reasons for the decision
    pub reasons: Vec<String>,

    /// Risk factors identified
    pub risk_factors: Vec<RiskFactor>,
}

/// Risk factors identified during analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFactor {
    /// Type of risk
    pub risk_type: RiskType,

    /// Severity (0.0 - 1.0)
    pub severity: f64,

    /// Description
    pub description: String,
}

/// Types of risks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskType {
    /// Low initial liquidity
    LowLiquidity,
    /// Known scam creator
    BlacklistedCreator,
    /// Token has mint authority
    MintAuthority,
    /// Token has freeze authority
    FreezeAuthority,
    /// High concentration of tokens
    HighConcentration,
    /// Similar to known rug
    SimilarToRug,
    /// Pool age too old
    PoolTooOld,
    /// Suspicious token metadata
    SuspiciousMetadata,
}

/// Execution result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Transaction signature
    pub signature: Signature,

    /// Was the transaction successful
    pub success: bool,

    /// Amount of tokens received
    pub tokens_received: Option<u64>,

    /// SOL spent (including fees)
    pub sol_spent_lamports: u64,

    /// Execution latency in milliseconds
    pub latency_ms: u64,

    /// Error message if failed
    pub error: Option<String>,

    /// Slot confirmed in
    pub confirmed_slot: Option<u64>,
}

/// Snipe attempt tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnipeAttempt {
    /// Unique ID for this attempt
    pub id: String,

    /// Target pool
    pub pool: Pool,

    /// Analysis result
    pub analysis: AnalysisResult,

    /// Execution result (if executed)
    pub execution: Option<ExecutionResult>,

    /// Attempt timestamp
    pub attempted_at: DateTime<Utc>,

    /// Current status
    pub status: SnipeStatus,
}

/// Status of a snipe attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnipeStatus {
    /// Detected, pending analysis
    Detected,
    /// Being analyzed
    Analyzing,
    /// Rejected by strategy
    Rejected,
    /// Approved, pending execution
    Approved,
    /// Transaction submitted
    Submitted,
    /// Transaction confirmed
    Confirmed,
    /// Transaction failed
    Failed,
}

impl fmt::Display for SnipeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnipeStatus::Detected => write!(f, "Detected"),
            SnipeStatus::Analyzing => write!(f, "Analyzing"),
            SnipeStatus::Rejected => write!(f, "Rejected"),
            SnipeStatus::Approved => write!(f, "Approved"),
            SnipeStatus::Submitted => write!(f, "Submitted"),
            SnipeStatus::Confirmed => write!(f, "Confirmed"),
            SnipeStatus::Failed => write!(f, "Failed"),
        }
    }
}

/// Well-known program IDs.
pub mod programs {
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    lazy_static::lazy_static! {
        /// SPL Token Program
        pub static ref TOKEN_PROGRAM: Pubkey =
            Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();

        /// SPL Token 2022 Program
        pub static ref TOKEN_2022_PROGRAM: Pubkey =
            Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap();

        /// Associated Token Account Program
        pub static ref ATA_PROGRAM: Pubkey =
            Pubkey::from_str("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL").unwrap();

        /// System Program
        pub static ref SYSTEM_PROGRAM: Pubkey =
            Pubkey::from_str("11111111111111111111111111111111").unwrap();

        /// pump.fun Program
        pub static ref PUMP_FUN_PROGRAM: Pubkey =
            Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").unwrap();

        /// Raydium AMM V4 Program
        pub static ref RAYDIUM_V4_PROGRAM: Pubkey =
            Pubkey::from_str("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8").unwrap();

        /// Raydium CPMM Program
        pub static ref RAYDIUM_CPMM_PROGRAM: Pubkey =
            Pubkey::from_str("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C").unwrap();

        /// Wrapped SOL mint
        pub static ref WSOL_MINT: Pubkey =
            Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
    }
}
