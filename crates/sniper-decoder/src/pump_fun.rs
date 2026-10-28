//! pump.fun instruction decoder.
//!
//! Decodes pump.fun bonding curve creation and swap instructions.
//! pump.fun uses a bonding curve mechanism where tokens are bought/sold
//! against a virtual liquidity pool until migration to Raydium.

use async_trait::async_trait;
use borsh::BorshDeserialize;
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

/// pump.fun instruction discriminators (first 8 bytes of instruction data),
/// from the current IDL (github.com/pump-fun/pump-public-docs, idl/pump.json).
pub mod discriminators {
    /// Legacy create (SPL Token mint + Metaplex metadata)
    pub const CREATE: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];
    /// create_v2 (Token-2022 mint); what pump.fun launches use today
    pub const CREATE_V2: [u8; 8] = [214, 144, 76, 236, 95, 139, 49, 180];

    pub const BUY: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
    pub const BUY_EXACT_SOL_IN: [u8; 8] = [56, 252, 116, 8, 158, 223, 205, 95];
    pub const BUY_V2: [u8; 8] = [184, 23, 238, 97, 103, 197, 211, 61];
    pub const BUY_V3: [u8; 8] = [7, 5, 29, 196, 245, 23, 101, 80];
    pub const BUY_EXACT_QUOTE_IN_V3: [u8; 8] = [225, 247, 80, 30, 213, 179, 132, 136];

    pub const SELL: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
    pub const SELL_V2: [u8; 8] = [93, 246, 130, 60, 231, 233, 64, 178];
    pub const SELL_V3: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];
}

/// Where `mint` and `bonding_curve` sit in each trade instruction's accounts.
fn trade_layout(d: [u8; 8]) -> Option<(usize, usize, bool, bool)> {
    use discriminators::*;
    // (mint idx, curve idx, is_buy, args are (quote_in, min_tokens_out))
    Some(match d {
        BUY => (2, 3, true, false),
        BUY_EXACT_SOL_IN => (2, 3, true, true),
        SELL => (2, 3, false, false),
        BUY_V2 => (1, 10, true, false),
        SELL_V2 => (1, 10, false, false),
        BUY_V3 => (1, 5, true, false),
        BUY_EXACT_QUOTE_IN_V3 => (1, 5, true, true),
        SELL_V3 => (1, 5, false, false),
        _ => return None,
    })
}

/// Leading create/create_v2 arguments. Both instructions start with these;
/// create_v2 appends more (mayhem flag, optional fields) which we read
/// separately.
#[derive(Debug, Clone, BorshDeserialize)]
pub struct CreateArgs {
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub creator: Pubkey,
}

/// Borsh-read `T` from the front of `data`, ignoring trailing bytes.
///
/// pump.fun keeps appending optional args (`track_volume`, `partial_fill`,
/// ...), so strict `try_from_slice` breaks every time the IDL grows.
fn read_prefix<T: BorshDeserialize>(data: &[u8], what: &str) -> Result<T> {
    let mut slice = data;
    T::deserialize(&mut slice).map_err(|e| Error::Decode(format!("Failed to parse {}: {}", what, e)))
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

    /// Parse `create` / `create_v2`.
    ///
    /// Account layouts (current IDL):
    /// - create:    0 mint, 1 mint_authority, 2 bonding_curve, 3 assoc_bonding_curve,
    ///   4 global, 5 mpl_token_metadata, 6 metadata, 7 user, 8 system, 9 token_program, ...
    /// - create_v2: 0 mint, 1 mint_authority, 2 bonding_curve, 3 assoc_bonding_curve,
    ///   4 global, 5 user, 6 system, 7 token_program (Token-2022), ... 15 program,
    ///   then optional remaining accounts 16 quote_mint, 17 assoc_quote_curve, 18 quote_token_program.
    fn parse_create_instruction(
        &self,
        data: &[u8],
        accounts: &[Pubkey],
        signature: solana_sdk::signature::Signature,
        slot: u64,
    ) -> Result<DecodedInstruction> {
        let is_v2 = data.len() >= 8 && data[..8] == discriminators::CREATE_V2;
        let mut rest = data
            .get(8..)
            .ok_or_else(|| Error::Decode("Instruction data too short".to_string()))?;
        let args = CreateArgs::deserialize(&mut rest)
            .map_err(|e| Error::Decode(format!("Failed to parse create args: {}", e)))?;

        let (min_accounts, token_program_idx) = if is_v2 { (8, 7) } else { (10, 9) };
        if accounts.len() < min_accounts {
            return Err(Error::Decode(format!(
                "Not enough accounts for create: {}",
                accounts.len()
            )));
        }

        let mint = accounts[0];
        let bonding_curve = accounts[2];
        let token_program = accounts[token_program_idx];
        // create_v2 appends `is_mayhem_mode: bool` right after `creator`.
        let is_mayhem_mode = is_v2 && rest.first().is_some_and(|b| *b != 0);
        // A non-WSOL quote mint in remaining account 16 means the coin is not SOL-paired.
        let quote_mint = match accounts.get(16) {
            Some(q) if is_v2 && *q != Pubkey::default() => *q,
            _ => *programs::WSOL_MINT,
        };

        info!(
            mint = %mint,
            name = %args.name,
            symbol = %args.symbol,
            creator = %args.creator,
            v2 = is_v2,
            mayhem = is_mayhem_mode,
            "Detected pump.fun token creation"
        );

        let pool = Pool {
            address: bonding_curve,
            token_mint: mint,
            quote_mint,
            pool_type: PoolType::PumpFun,
            initial_liquidity_lamports: 0, // Will be updated from on-chain data
            token_reserve: 0,
            creation_slot: slot,
            created_at: Utc::now(),
            creator: Some(args.creator),
            bonding_curve: Some(bonding_curve),
            token_program: Some(token_program),
            is_mayhem_mode,
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
                discriminators::CREATE | discriminators::CREATE_V2 => {
                    self.parse_create_instruction(&ix.data, &accounts, signature, slot)
                }
                d => match trade_layout(d) {
                    Some(layout) => parse_trade(&ix.data, &accounts, layout),
                    None => Ok(DecodedInstruction::Unknown),
                },
            };
            match decoded {
                Ok(d) => out.push(d),
                Err(e) => debug!(error = %e, "Skipping malformed pump.fun instruction"),
            }
        }
        out
    }

}

/// Parse any buy/sell variant. Every variant's first two args are u64s:
/// `(amount, max_sol_cost)` for buys, `(amount, min_sol_output)` for sells,
/// or `(quote_in, min_tokens_out)` for exact-in buys.
fn parse_trade(
    data: &[u8],
    accounts: &[Pubkey],
    (mint_idx, curve_idx, is_buy, exact_in): (usize, usize, bool, bool),
) -> Result<DecodedInstruction> {
    let (a, b): (u64, u64) = read_prefix(&data[8..], "trade args")?;
    let (mint, pool) = match (accounts.get(mint_idx), accounts.get(curve_idx)) {
        (Some(m), Some(c)) => (*m, *c),
        _ => return Err(Error::Decode(format!("Not enough accounts for trade: {}", accounts.len()))),
    };
    // Normalise to (amount_in, amount_out) in the direction of the trade.
    let (amount_in, amount_out) = match (is_buy, exact_in) {
        (true, false) => (b, a),  // max_sol_cost in, tokens out
        (true, true) => (a, b),   // quote in, min tokens out
        (false, _) => (a, b),     // tokens in, min sol out
    };
    debug!(mint = %mint, is_buy, amount_in, amount_out, "Detected pump.fun trade");
    Ok(DecodedInstruction::Swap {
        pool,
        token_mint: mint,
        amount_in,
        amount_out,
        is_buy,
    })
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
            let vtx = sniper_core::txv1::deserialize_any(&tx.data)?;
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
            // 1 = accept SIMD-0385 v1 transactions, which pump.fun launches use.
            max_supported_transaction_version: Some(1),
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
        let vtx = match &fetched.transaction.transaction {
            solana_transaction_status::EncodedTransaction::Binary(b64, _) => {
                let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
                    .map_err(|e| Error::Decode(format!("bad base64 tx: {}", e)))?;
                sniper_core::txv1::deserialize_any(&raw)?
            }
            other => other
                .decode()
                .ok_or_else(|| Error::Decode("Failed to decode fetched transaction".to_string()))?,
        };
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

        let curve_data = sniper_core::BondingCurveState::from_account_data(&account.data)?;

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
            token_program: None,
            is_mayhem_mode: false,
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

    const CREATOR: Pubkey = Pubkey::new_from_array([7; 32]);

    fn create_data() -> Vec<u8> {
        let mut d = discriminators::CREATE.to_vec();
        borsh_string(&mut d, "Test");
        borsh_string(&mut d, "TST");
        borsh_string(&mut d, "https://x");
        d.extend_from_slice(CREATOR.as_ref());
        d
    }

    fn buy_data(amount: u64, max: u64) -> Vec<u8> {
        let mut d = discriminators::BUY.to_vec();
        d.extend_from_slice(&amount.to_le_bytes());
        d.extend_from_slice(&max.to_le_bytes());
        d.push(1); // trailing track_volume OptionBool, as mainnet sends
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
                // Creator comes from the instruction args, not the payer.
                assert_eq!(ev.pool.creator, Some(CREATOR));
                assert_eq!(ev.pool.token_program, Some(accts[9]));
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
                accounts: vec![2, 5, 3, 5, 5, 5, 5, 4, 5, 5],
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
                assert_eq!(ev.pool.creator, Some(CREATOR));
                let _ = creator;
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

    /// Real mainnet launch (v1 tx: create_v2 + buy_v2), captured 2026-10-02.
    #[tokio::test]
    async fn decodes_real_mainnet_create_v2_launch() {
        use base64::Engine;
        use std::str::FromStr;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(include_str!("../../sniper-core/tests/fixtures/create_v2_tx_v1.b64").trim())
            .unwrap();
        let vtx = sniper_core::txv1::deserialize_any(&raw).unwrap();
        let parsed = ParsedTransaction {
            signature: vtx.signatures[0],
            slot: 1,
            block_time: None,
            data: raw,
            account_keys: vec![], // v1 has no lookup tables; static keys suffice
            program_ids: vec![],
            success: true,
        };
        let out = decoder().decode(&parsed).await.unwrap();
        let pk = |s: &str| Pubkey::from_str(s).unwrap();
        match &out[..] {
            [DecodedInstruction::PoolCreation(ev), DecodedInstruction::Swap { is_buy: true, token_mint, pool, .. }] => {
                let mint = pk("BpDR9CZtZKYMu1a9L8yBfdXVJx9NcbZjkaVYSRq8pump");
                let curve = pk("FjzSWshoB8msrGDpYnREYKYSpxoreQXTy5WvSCu8wq84");
                assert_eq!(ev.pool.token_mint, mint);
                assert_eq!(ev.pool.bonding_curve, Some(curve));
                assert_eq!(ev.pool.bonding_curve.unwrap(), PumpFunDecoder::derive_bonding_curve(&mint).0);
                assert_eq!(ev.pool.creator, Some(pk("FhTPy8WsprLrbetyEAUJYYaXfChrKa2GGNxygorBMNC")));
                assert_eq!(ev.pool.token_program, Some(*programs::TOKEN_2022_PROGRAM));
                assert_eq!(ev.pool.quote_mint, *programs::WSOL_MINT);
                assert!(!ev.pool.is_mayhem_mode);
                // The dev buy in the same tx decodes against the same curve.
                assert_eq!(*token_mint, mint);
                assert_eq!(*pool, curve);
            }
            other => panic!("unexpected {:?}", other),
        }
    }

    #[test]
    fn decodes_v3_trade_layouts() {
        let accts: Vec<Pubkey> = (0..17).map(|_| Pubkey::new_unique()).collect();
        let mut sell = discriminators::SELL_V3.to_vec();
        sell.extend_from_slice(&500u64.to_le_bytes());
        sell.extend_from_slice(&9u64.to_le_bytes());
        let mut exact = discriminators::BUY_EXACT_QUOTE_IN_V3.to_vec();
        exact.extend_from_slice(&1_000u64.to_le_bytes());
        exact.extend_from_slice(&42u64.to_le_bytes());
        exact.push(1);
        exact.push(0);
        let (vtx, keys) = tx(&[ix(accts.clone(), sell), ix(accts.clone(), exact)]);
        let out = decoder().decode_versioned(&vtx, &keys, vtx.signatures[0], 1);
        match &out[..] {
            [DecodedInstruction::Swap { is_buy: false, amount_in: 500, amount_out: 9, token_mint: m1, pool: c1 },
             DecodedInstruction::Swap { is_buy: true, amount_in: 1_000, amount_out: 42, .. }] => {
                assert_eq!(*m1, accts[1]);
                assert_eq!(*c1, accts[5]);
            }
            other => panic!("{:?}", other),
        }
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
