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
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::{option_serializer::OptionSerializer, UiTransactionStatusMeta};
use std::str::FromStr;
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
    #[allow(dead_code)]
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

    /// Decode every pump.fun instruction in an already-materialised
    /// transaction. `account_keys` must be the full resolved key list
    /// (static keys, then loaded writable, then loaded readonly).
    ///
    /// Malformed instructions are skipped, not fatal, so one bad
    /// instruction can't hide a pool creation in the same transaction.
    pub fn decode_versioned(
        &self,
        vtx: &VersionedTransaction,
        account_keys: &[Pubkey],
        signature: solana_sdk::signature::Signature,
        slot: u64,
    ) -> Vec<DecodedInstruction> {
        let mut out = Vec::new();
        for ix in vtx.message.instructions() {
            let Some(program_id) = account_keys.get(ix.program_id_index as usize) else {
                continue;
            };
            if *program_id != *programs::PUMP_FUN_PROGRAM || ix.data.len() < 8 {
                continue;
            }
            let Some(accounts) = ix
                .accounts
                .iter()
                .map(|&i| account_keys.get(i as usize).copied())
                .collect::<Option<Vec<Pubkey>>>()
            else {
                debug!("Instruction references unresolved account index");
                continue;
            };
            let discriminator: [u8; 8] = ix.data[..8].try_into().expect("len checked");
            let decoded = match discriminator {
                discriminators::CREATE => {
                    self.parse_create_instruction(&ix.data, &accounts, signature, slot)
                }
                discriminators::BUY => self.parse_buy_instruction(&ix.data, &accounts),
                discriminators::SELL => self.parse_sell_instruction(&ix.data, &accounts),
                _ => Ok(DecodedInstruction::Unknown),
            };
            match decoded {
                Ok(d) => out.push(d),
                Err(e) => debug!(error = %e, "Skipping malformed pump.fun instruction"),
            }
        }
        out
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
        if !tx.success {
            return Ok(vec![]);
        }

        // Geyser delivers the full transaction; logsSubscribe only gives us
        // a signature, so fetch it in that case.
        if !tx.data.is_empty() {
            let vtx: VersionedTransaction = bincode::deserialize(&tx.data)
                .map_err(|e| Error::Decode(format!("Failed to deserialize tx: {}", e)))?;
            let keys = if tx.account_keys.is_empty() {
                vtx.message.static_account_keys().to_vec()
            } else {
                tx.account_keys.clone()
            };
            return Ok(self.decode_versioned(&vtx, &keys, tx.signature, tx.slot));
        }

        let config = solana_client::rpc_config::RpcTransactionConfig {
            encoding: Some(solana_transaction_status::UiTransactionEncoding::Base64),
            commitment: Some(solana_sdk::commitment_config::CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        };
        let fetched = match self
            .rpc_client
            .get_transaction_with_config(&tx.signature, config)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, "Failed to fetch transaction");
                return Ok(vec![]);
            }
        };

        let mut loaded = Vec::new();
        if let Some(meta) = &fetched.transaction.meta {
            if meta.err.is_some() {
                return Ok(vec![]);
            }
            loaded = loaded_addresses(meta)?;
        }
        let vtx = fetched
            .transaction
            .transaction
            .decode()
            .ok_or_else(|| Error::Decode("Failed to decode fetched transaction".to_string()))?;
        let mut keys = vtx.message.static_account_keys().to_vec();
        keys.extend(loaded);
        Ok(self.decode_versioned(&vtx, &keys, tx.signature, fetched.slot))
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

/// Address-lookup-table keys from RPC metadata, writable first.
fn loaded_addresses(meta: &UiTransactionStatusMeta) -> Result<Vec<Pubkey>> {
    match &meta.loaded_addresses {
        OptionSerializer::Some(la) => la
            .writable
            .iter()
            .chain(la.readonly.iter())
            .map(|s| Pubkey::from_str(s).map_err(Error::from))
            .collect(),
        _ => Ok(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    fn decoder() -> PumpFunDecoder {
        PumpFunDecoder::new(Arc::new(RpcClient::new("http://127.0.0.1:1".to_string())))
    }

    fn borsh_string(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    fn create_data() -> Vec<u8> {
        let mut d = discriminators::CREATE.to_vec();
        borsh_string(&mut d, "Test");
        borsh_string(&mut d, "TST");
        borsh_string(&mut d, "https://x");
        d
    }

    fn buy_data(amount: u64, max: u64) -> Vec<u8> {
        let mut d = discriminators::BUY.to_vec();
        d.extend_from_slice(&amount.to_le_bytes());
        d.extend_from_slice(&max.to_le_bytes());
        d
    }

    fn ix(accounts: Vec<Pubkey>, data: Vec<u8>) -> solana_sdk::instruction::Instruction {
        solana_sdk::instruction::Instruction {
            program_id: *programs::PUMP_FUN_PROGRAM,
            accounts: accounts
                .into_iter()
                .map(|k| solana_sdk::instruction::AccountMeta::new(k, false))
                .collect(),
            data,
        }
    }

    fn tx(ixs: &[solana_sdk::instruction::Instruction]) -> (VersionedTransaction, Vec<Pubkey>) {
        use solana_sdk::{signature::Keypair, signer::Signer};
        let kp = Keypair::new();
        let msg = solana_sdk::message::Message::new(ixs, Some(&kp.pubkey()));
        let mut t = solana_sdk::transaction::Transaction::new_unsigned(msg);
        t.sign(&[&kp], solana_sdk::hash::Hash::new_unique());
        let keys = t.message.account_keys.clone();
        (t.into(), keys)
    }

    #[test]
    fn decodes_create_and_buy_locally() {
        let accts: Vec<Pubkey> = (0..12).map(|_| Pubkey::new_unique()).collect();
        let (vtx, keys) = tx(&[ix(accts.clone(), create_data()), ix(accts.clone(), buy_data(5, 9))]);
        let sig = vtx.signatures[0];
        let out = decoder().decode_versioned(&vtx, &keys, sig, 77);
        assert_eq!(out.len(), 2);
        match &out[0] {
            DecodedInstruction::PoolCreation(ev) => {
                assert_eq!(ev.pool.token_mint, accts[0]);
                assert_eq!(ev.pool.bonding_curve, Some(accts[2]));
                assert_eq!(ev.pool.creator, Some(accts[7]));
                assert_eq!(ev.slot, 77);
            }
            other => panic!("{:?}", other),
        }
        assert!(matches!(
            out[1],
            DecodedInstruction::Swap { amount_out: 5, amount_in: 9, is_buy: true, .. }
        ));
    }

    #[test]
    fn malformed_instruction_does_not_hide_others() {
        let accts: Vec<Pubkey> = (0..12).map(|_| Pubkey::new_unique()).collect();
        let bad = discriminators::CREATE.to_vec(); // no args
        let (vtx, keys) = tx(&[ix(accts.clone(), bad), ix(accts, create_data())]);
        let out = decoder().decode_versioned(&vtx, &keys, vtx.signatures[0], 1);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], DecodedInstruction::PoolCreation(_)));
    }

    #[test]
    fn resolves_lookup_table_accounts() {
        use solana_sdk::message::{v0, MessageHeader, VersionedMessage};
        use solana_sdk::signature::Keypair;
        let kp = Keypair::new();
        let mint = Pubkey::new_unique();
        let curve = Pubkey::new_unique();
        let creator = Pubkey::new_unique();
        // Static: payer(0), program(1). Loaded via ALT: mint(2), curve(3), creator(4).
        let filler = Pubkey::new_unique();
        let msg = v0::Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![solana_sdk::signer::Signer::pubkey(&kp), *programs::PUMP_FUN_PROGRAM],
            recent_blockhash: solana_sdk::hash::Hash::new_unique(),
            instructions: vec![solana_sdk::instruction::CompiledInstruction {
                program_id_index: 1,
                accounts: vec![2, 5, 3, 5, 5, 5, 5, 4],
                data: create_data(),
            }],
            address_table_lookups: vec![v0::MessageAddressTableLookup {
                account_key: Pubkey::new_unique(),
                writable_indexes: vec![0, 1, 2],
                readonly_indexes: vec![3],
            }],
        };
        let vtx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&kp]).unwrap();
        let mut keys = vtx.message.static_account_keys().to_vec();
        keys.extend([mint, curve, creator, filler]);
        let out = decoder().decode_versioned(&vtx, &keys, vtx.signatures[0], 1);
        match &out[..] {
            [DecodedInstruction::PoolCreation(ev)] => {
                assert_eq!(ev.pool.token_mint, mint);
                assert_eq!(ev.pool.bonding_curve, Some(curve));
                assert_eq!(ev.pool.creator, Some(creator));
            }
            other => panic!("{:?}", other),
        }
        // Without the loaded keys the instruction is skipped, not a panic.
        let static_only = vtx.message.static_account_keys().to_vec();
        assert!(decoder().decode_versioned(&vtx, &static_only, vtx.signatures[0], 1).is_empty());
    }

    #[tokio::test]
    async fn decode_uses_embedded_data_without_rpc() {
        // RPC points at a dead port: success proves no fetch happened.
        let accts: Vec<Pubkey> = (0..12).map(|_| Pubkey::new_unique()).collect();
        let (vtx, keys) = tx(&[ix(accts, create_data())]);
        let parsed = ParsedTransaction {
            signature: vtx.signatures[0],
            slot: 5,
            block_time: None,
            data: bincode::serialize(&vtx).unwrap(),
            account_keys: keys,
            program_ids: vec![*programs::PUMP_FUN_PROGRAM],
            success: true,
        };
        let out = decoder().decode(&parsed).await.unwrap();
        assert!(matches!(out[..], [DecodedInstruction::PoolCreation(_)]));

        let mut failed = parsed.clone();
        failed.success = false;
        assert!(decoder().decode(&failed).await.unwrap().is_empty());
    }

    #[test]
    fn test_derive_bonding_curve() {
        // Test PDA derivation
        let mint = Pubkey::new_unique();
        let (pda, bump) = PumpFunDecoder::derive_bonding_curve(&mint);
        assert!(bump > 0);
        assert_ne!(pda, mint);
    }
}
