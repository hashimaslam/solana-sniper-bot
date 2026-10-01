//! pump.fun instruction decoder.
//!
//! Decodes pump.fun bonding curve creation and swap instructions.
//! pump.fun uses a bonding curve mechanism where tokens are bought/sold
//! against a virtual liquidity pool until migration to Raydium.

use async_trait::async_trait;
use borsh::{BorshDeserialize, BorshSerialize};
use chrono::Utc;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use tracing::{debug, info, warn};

use sniper_core::{programs, Error, ParsedTransaction, Pool, PoolCreationEvent, PoolType, Result};

use crate::traits::{DecodedInstruction, Decoder};

/// pump.fun instruction discriminators (first 8 bytes of instruction data).
mod discriminators {
    /// Create bonding curve (new token launch)
    pub const CREATE: [u8; 8] = [0x18, 0x1e, 0xc8, 0x28, 0x05, 0x1c, 0x07, 0x77];

    /// Buy tokens from bonding curve
    pub const BUY: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];

    /// Sell tokens to bonding curve
    pub const SELL: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

    /// Withdraw liquidity (migration to Raydium)
    pub const WITHDRAW: [u8; 8] = [0xb7, 0x12, 0x46, 0x9c, 0x94, 0x6d, 0xa1, 0x22];
}

/// pump.fun bonding curve account data.
#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
pub struct BondingCurveData {
    /// Discriminator (8 bytes)
    pub discriminator: [u8; 8],
    /// Virtual token reserves
    pub virtual_token_reserves: u64,
    /// Virtual SOL reserves
    pub virtual_sol_reserves: u64,
    /// Real token reserves
    pub real_token_reserves: u64,
    /// Real SOL reserves
    pub real_sol_reserves: u64,
    /// Token total supply
    pub token_total_supply: u64,
    /// Whether the curve is complete (migrated)
    pub complete: bool,
}

/// Create instruction arguments.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct CreateArgs {
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

/// Buy instruction arguments.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct BuyArgs {
    pub amount: u64,
    pub max_sol_cost: u64,
}

/// Sell instruction arguments.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct SellArgs {
    pub amount: u64,
    pub min_sol_output: u64,
}

/// pump.fun decoder implementation.
pub struct PumpFunDecoder {
    rpc_client: Arc<RpcClient>,
}

impl PumpFunDecoder {
    /// Create a new pump.fun decoder.
    pub fn new(rpc_client: Arc<RpcClient>) -> Self {
        Self { rpc_client }
    }

    /// Derive the bonding curve PDA for a token mint.
    pub fn derive_bonding_curve(mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[b"bonding-curve", mint.as_ref()],
            &programs::PUMP_FUN_PROGRAM,
        )
    }

    /// Derive the associated bonding curve token account.
    pub fn derive_bonding_curve_ata(bonding_curve: &Pubkey, mint: &Pubkey) -> Pubkey {
        spl_associated_token_account::get_associated_token_address(bonding_curve, mint)
    }

    /// Parse create instruction.
    fn parse_create_instruction(
        &self,
        data: &[u8],
        accounts: &[Pubkey],
        signature: solana_sdk::signature::Signature,
        slot: u64,
    ) -> Result<DecodedInstruction> {
        if data.len() < 8 {
            return Err(Error::Decode("Instruction data too short".to_string()));
        }

        // Skip discriminator and parse args
        let args: CreateArgs = BorshDeserialize::try_from_slice(&data[8..])
            .map_err(|e| Error::Decode(format!("Failed to parse create args: {}", e)))?;

        // Account layout for create instruction:
        // 0: mint
        // 1: mint authority (PDA)
        // 2: bonding curve
        // 3: associated bonding curve (token account)
        // 4: global state
        // 5: mpl token metadata
        // 6: metadata account
        // 7: user
        // 8: system program
        // 9: token program
        // 10: associated token program
        // 11: rent

        if accounts.len() < 8 {
            return Err(Error::Decode("Not enough accounts for create".to_string()));
        }

        let mint = accounts[0];
        let bonding_curve = accounts[2];
        let creator = accounts[7];

        info!(
            mint = %mint,
            name = %args.name,
            symbol = %args.symbol,
            creator = %creator,
            "Detected pump.fun token creation"
        );

        let pool = Pool {
            address: bonding_curve,
            token_mint: mint,
            quote_mint: *programs::WSOL_MINT,
            pool_type: PoolType::PumpFun,
            initial_liquidity_lamports: 0, // Will be updated from on-chain data
            token_reserve: 0,
            creation_slot: slot,
            created_at: Utc::now(),
            creator: Some(creator),
            bonding_curve: Some(bonding_curve),
        };

        Ok(DecodedInstruction::PoolCreation(PoolCreationEvent {
            pool,
            signature,
            slot,
            instruction_data: data.to_vec(),
        }))
    }

    /// Parse buy instruction.
    fn parse_buy_instruction(
        &self,
        data: &[u8],
        accounts: &[Pubkey],
    ) -> Result<DecodedInstruction> {
        if data.len() < 8 + 16 {
            return Err(Error::Decode("Buy instruction data too short".to_string()));
        }

        let args: BuyArgs = BorshDeserialize::try_from_slice(&data[8..])
            .map_err(|e| Error::Decode(format!("Failed to parse buy args: {}", e)))?;

        // Account layout for buy:
        // 0: global
        // 1: fee recipient
        // 2: mint
        // 3: bonding curve
        // 4: associated bonding curve
        // 5: associated user
        // 6: user
        // 7: system program
        // 8: token program
        // 9: rent
        // 10: event authority
        // 11: program

        if accounts.len() < 4 {
            return Err(Error::Decode("Not enough accounts for buy".to_string()));
        }

        let mint = accounts[2];
        let bonding_curve = accounts[3];

        debug!(
            mint = %mint,
            amount = args.amount,
            max_cost = args.max_sol_cost,
            "Detected pump.fun buy"
        );

        Ok(DecodedInstruction::Swap {
            pool: bonding_curve,
            token_mint: mint,
            amount_in: args.max_sol_cost,
            amount_out: args.amount,
            is_buy: true,
        })
    }

    /// Parse sell instruction.
    fn parse_sell_instruction(
        &self,
        data: &[u8],
        accounts: &[Pubkey],
    ) -> Result<DecodedInstruction> {
        if data.len() < 8 + 16 {
            return Err(Error::Decode("Sell instruction data too short".to_string()));
        }

        let args: SellArgs = BorshDeserialize::try_from_slice(&data[8..])
            .map_err(|e| Error::Decode(format!("Failed to parse sell args: {}", e)))?;

        if accounts.len() < 4 {
            return Err(Error::Decode("Not enough accounts for sell".to_string()));
        }

        let mint = accounts[2];
        let bonding_curve = accounts[3];

        debug!(
            mint = %mint,
            amount = args.amount,
            min_output = args.min_sol_output,
            "Detected pump.fun sell"
        );

        Ok(DecodedInstruction::Swap {
            pool: bonding_curve,
            token_mint: mint,
            amount_in: args.amount,
            amount_out: args.min_sol_output,
            is_buy: false,
        })
    }
}

#[async_trait]
impl Decoder for PumpFunDecoder {
    fn program_id(&self) -> Pubkey {
        *programs::PUMP_FUN_PROGRAM
    }

    async fn decode(&self, tx: &ParsedTransaction) -> Result<Vec<DecodedInstruction>> {
        let mut instructions = Vec::new();

        // For now, we need to fetch the full transaction to get instruction data
        // This is because ParsedTransaction only has signature from logs subscription
        if tx.data.is_empty() {
            // Fetch full transaction
            let config = solana_client::rpc_config::RpcTransactionConfig {
                encoding: Some(solana_transaction_status::UiTransactionEncoding::Base64),
                commitment: Some(solana_sdk::commitment_config::CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            };

            match self
                .rpc_client
                .get_transaction_with_config(&tx.signature, config)
                .await
            {
                Ok(tx_data) => {
                    if let Some(meta) = tx_data.transaction.meta {
                        if meta.err.is_some() {
                            return Ok(vec![]);
                        }
                    }

                    // Decode the transaction
                    if let solana_transaction_status::EncodedTransaction::Binary(data, _) =
                        tx_data.transaction.transaction
                    {
                        let decoded = bs58::decode(&data)
                            .into_vec()
                            .or_else(|_| base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &data))
                            .map_err(|e| Error::Decode(format!("Failed to decode tx: {}", e)))?;

                        // Parse the versioned transaction
                        if let Ok(versioned_tx) =
                            bincode::deserialize::<solana_sdk::transaction::VersionedTransaction>(
                                &decoded,
                            )
                        {
                            let message = versioned_tx.message;
                            let account_keys = message.static_account_keys();

                            for instruction in message.instructions() {
                                let program_id = account_keys[instruction.program_id_index as usize];

                                if program_id != *programs::PUMP_FUN_PROGRAM {
                                    continue;
                                }

                                let data = &instruction.data;
                                if data.len() < 8 {
                                    continue;
                                }

                                let discriminator: [u8; 8] = data[..8].try_into().unwrap();
                                let accounts: Vec<Pubkey> = instruction
                                    .accounts
                                    .iter()
                                    .map(|&i| account_keys[i as usize])
                                    .collect();

                                let decoded_ix = match discriminator {
                                    discriminators::CREATE => {
                                        self.parse_create_instruction(data, &accounts, tx.signature, tx.slot)?
                                    }
                                    discriminators::BUY => {
                                        self.parse_buy_instruction(data, &accounts)?
                                    }
                                    discriminators::SELL => {
                                        self.parse_sell_instruction(data, &accounts)?
                                    }
                                    _ => {
                                        debug!("Unknown pump.fun instruction");
                                        DecodedInstruction::Unknown
                                    }
                                };

                                instructions.push(decoded_ix);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to fetch transaction");
                }
            }
        }

        Ok(instructions)
    }

    async fn fetch_pool(&self, pool_address: &Pubkey) -> Result<Pool> {
        let account = self
            .rpc_client
            .get_account(pool_address)
            .await
            .map_err(|e| Error::Rpc(format!("Failed to fetch bonding curve: {}", e)))?;

        let curve_data: BondingCurveData = BorshDeserialize::try_from_slice(&account.data)
            .map_err(|e| Error::Decode(format!("Failed to parse bonding curve: {}", e)))?;

        // Derive mint from PDA (reverse lookup - need to iterate or cache)
        // For now, return a partial pool
        Ok(Pool {
            address: *pool_address,
            token_mint: Pubkey::default(), // Would need mint tracking
            quote_mint: *programs::WSOL_MINT,
            pool_type: PoolType::PumpFun,
            initial_liquidity_lamports: curve_data.real_sol_reserves,
            token_reserve: curve_data.real_token_reserves,
            creation_slot: 0,
            created_at: Utc::now(),
            creator: None,
            bonding_curve: Some(*pool_address),
        })
    }

    fn name(&self) -> &'static str {
        "pump.fun"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_bonding_curve() {
        // Test PDA derivation
        let mint = Pubkey::new_unique();
        let (pda, bump) = PumpFunDecoder::derive_bonding_curve(&mint);
        assert!(bump > 0);
        assert_ne!(pda, mint);
    }
}
