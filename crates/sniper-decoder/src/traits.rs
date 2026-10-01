//! Decoder trait definition.

use async_trait::async_trait;
use sniper_core::{ParsedTransaction, Pool, PoolCreationEvent, Result};
use solana_sdk::pubkey::Pubkey;

/// Decoded instruction from a transaction.
#[derive(Debug, Clone)]
pub enum DecodedInstruction {
    /// Pool/bonding curve creation
    PoolCreation(PoolCreationEvent),
    /// Token swap/buy
    Swap {
        pool: Pubkey,
        token_mint: Pubkey,
        amount_in: u64,
        amount_out: u64,
        is_buy: bool,
    },
    /// Liquidity addition
    AddLiquidity {
        pool: Pubkey,
        token_amount: u64,
        sol_amount: u64,
    },
    /// Liquidity removal
    RemoveLiquidity {
        pool: Pubkey,
        token_amount: u64,
        sol_amount: u64,
    },
    /// Unknown instruction
    Unknown,
}

/// Trait for transaction/instruction decoders.
#[async_trait]
pub trait Decoder: Send + Sync {
    /// Get the program ID this decoder handles.
    fn program_id(&self) -> Pubkey;

    /// Check if this decoder can handle the given program.
    fn can_decode(&self, program_id: &Pubkey) -> bool {
        *program_id == self.program_id()
    }

    /// Decode a transaction and extract relevant instructions.
    async fn decode(&self, tx: &ParsedTransaction) -> Result<Vec<DecodedInstruction>>;

    /// Fetch full pool details from chain.
    async fn fetch_pool(&self, pool_address: &Pubkey) -> Result<Pool>;

    /// Get the decoder name.
    fn name(&self) -> &'static str;
}
