//! pump.fun executor implementation.
//!
//! Builds and executes buy/sell transactions against pump.fun bonding curves.
//! Pricing uses the live curve state (falling back to launch reserves when the
//! account isn't visible yet), and submission goes through a pluggable
//! [`TxSender`] so Jito bundles can be swapped in.
//!
//! NOTE: account layouts below follow the original pump.fun IDL. pump.fun has
//! added accounts over time (creator vault, volume accumulators); verify the
//! layout against the current IDL before trading real funds.

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

use sniper_core::{
    config::ExecutionConfig,
    curve::{self, BondingCurveState},
    programs, Error, ExecutionResult, Pool, Result,
};

use crate::sender::{RpcSender, TxSender};
use crate::transaction::{Executor, TransactionBuilder};

/// Instruction discriminators (current IDL).
pub mod discriminators {
    /// `buy_exact_quote_in_v3(spendable_quote_in, min_tokens_out, partial_fill)`
    pub const BUY_EXACT_QUOTE_IN_V3: [u8; 8] = [225, 247, 80, 30, 213, 179, 132, 136];
    /// `sell_v3(amount, min_sol_output)`
    pub const SELL_V3: [u8; 8] = [28, 146, 222, 119, 38, 196, 105, 213];
}

/// Pump Fees program.
pub const FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";

/// Buyback fee recipients. v3 trades on SOL-paired coins pass one of these
/// wallets directly. Source: pump-public-docs FEE_RECIPIENTS.md.
pub const BUYBACK_FEE_RECIPIENTS: [&str; 8] = [
    "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD",
    "9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7",
    "GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL",
    "3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR",
    "5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6",
    "EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL",
    "5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD",
    "A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW",
];

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).expect("valid const pubkey")
}

fn random_buyback_recipient() -> Pubkey {
    use rand::seq::SliceRandom;
    pk(BUYBACK_FEE_RECIPIENTS
        .choose(&mut rand::thread_rng())
        .expect("non-empty"))
}

/// The 17 accounts shared by `buy_v3`, `buy_exact_quote_in_v3` and `sell_v3`,
/// for a SOL-paired coin.
///
/// Layout verified against mainnet `sell_v3` transaction
/// 2eyyahVtB6SP8dLgjvmePYXo5H9xCgmW2DamMJ4Lo73uf3QMRtuLGmjd4sZnzgWp9PXyMZ7QgDA8o43WTw9yF4LW
/// (see the `matches_mainnet_sell_v3_accounts` test).
pub fn v3_accounts(user: &Pubkey, pool: &Pool, buyback_recipient: Pubkey) -> Result<Vec<AccountMeta>> {
    let program = *programs::PUMP_FUN_PROGRAM;
    let mint = pool.token_mint;
    let bonding_curve = pool
        .bonding_curve
        .ok_or_else(|| Error::Execution("No bonding curve address".to_string()))?;
    let base_token_program = pool.token_program();
    let quote_mint = *programs::WSOL_MINT;
    let quote_token_program = *programs::TOKEN_PROGRAM;
    let ata = spl_associated_token_account::get_associated_token_address_with_program_id;
    let fee_program = pk(FEE_PROGRAM);

    Ok(vec![
        AccountMeta::new_readonly(Pubkey::find_program_address(&[b"global"], &program).0, false),
        AccountMeta::new_readonly(mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new_readonly(base_token_program, false),
        AccountMeta::new_readonly(quote_token_program, false),
        AccountMeta::new(bonding_curve, false),
        AccountMeta::new(ata(&bonding_curve, &mint, &base_token_program), false),
        // Not used for SOL-paired coins, but must be the derived address.
        AccountMeta::new(ata(&bonding_curve, &quote_mint, &quote_token_program), false),
        AccountMeta::new(*user, true),
        AccountMeta::new(ata(user, &mint, &base_token_program), false),
        AccountMeta::new(ata(user, &quote_mint, &quote_token_program), false),
        AccountMeta::new(
            Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], &program).0,
            false,
        ),
        AccountMeta::new_readonly(
            Pubkey::find_program_address(&[b"fee_config", program.as_ref()], &fee_program).0,
            false,
        ),
        AccountMeta::new(buyback_recipient, false),
        AccountMeta::new_readonly(*programs::SYSTEM_PROGRAM, false),
        AccountMeta::new_readonly(
            Pubkey::find_program_address(&[b"__event_authority"], &program).0,
            false,
        ),
        AccountMeta::new_readonly(program, false),
    ])
}

/// Build `buy_exact_quote_in_v3`: spend exactly `quote_in` lamports (fees
/// included), require at least `min_tokens_out`.
pub fn build_buy_instruction(
    user: &Pubkey,
    pool: &Pool,
    quote_in: u64,
    min_tokens_out: u64,
    buyback_recipient: Pubkey,
) -> Result<Instruction> {
    let mut data = discriminators::BUY_EXACT_QUOTE_IN_V3.to_vec();
    data.extend_from_slice(&quote_in.to_le_bytes());
    data.extend_from_slice(&min_tokens_out.to_le_bytes());
    // partial_fill: OptionBool = Some(false). Only matters for mayhem coins,
    // where `false` makes an over-sized buy fail instead of partially fill.
    data.extend_from_slice(&[1, 0]);
    Ok(Instruction {
        program_id: *programs::PUMP_FUN_PROGRAM,
        accounts: v3_accounts(user, pool, buyback_recipient)?,
        data,
    })
}

/// Build `sell_v3`.
pub fn build_sell_instruction(
    user: &Pubkey,
    pool: &Pool,
    token_amount: u64,
    min_sol_output: u64,
    buyback_recipient: Pubkey,
) -> Result<Instruction> {
    let mut data = discriminators::SELL_V3.to_vec();
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());
    Ok(Instruction {
        program_id: *programs::PUMP_FUN_PROGRAM,
        accounts: v3_accounts(user, pool, buyback_recipient)?,
        data,
    })
}

/// Compute `(quote_in, expected_tokens, min_tokens_out)` for spending `sol_amount`.
pub fn buy_params(curve: &BondingCurveState, sol_amount: f64, slippage: f64) -> (u64, u64, u64) {
    let quote_in = (sol_amount * 1e9) as u64;
    let expected = curve.buy_quote(quote_in, curve::DEFAULT_FEE_BPS);
    (quote_in, expected, curve::with_slippage_down(expected, slippage))
}

/// Compute `min_sol_output` for selling `tokens`.
pub fn sell_params(curve: &BondingCurveState, tokens: u64, slippage: f64) -> u64 {
    curve::with_slippage_down(curve.sell_quote(tokens, curve::DEFAULT_FEE_BPS), slippage)
}

/// Refuse curves this executor can't trade correctly.
pub fn check_tradeable(curve: &BondingCurveState) -> Result<()> {
    if curve.complete {
        return Err(Error::Execution("Bonding curve complete".into()));
    }
    if !curve.is_sol_quoted() {
        return Err(Error::Execution("Curve is not SOL-quoted".into()));
    }
    if curve.is_cashback_coin {
        // v3 trades fail with CashbackCoinNotSupported (6094) on these.
        return Err(Error::Execution("Cashback coins are not supported".into()));
    }
    Ok(())
}

/// pump.fun executor for buying and selling tokens.
pub struct PumpFunExecutor {
    rpc_client: Arc<RpcClient>,
    wallet: Arc<Keypair>,
    config: ExecutionConfig,
    sender: Arc<dyn TxSender>,
}

impl PumpFunExecutor {
    /// Create a new pump.fun executor that sends through plain RPC.
    pub fn new(rpc_client: Arc<RpcClient>, wallet: Arc<Keypair>, config: ExecutionConfig) -> Self {
        let sender = Arc::new(RpcSender::new(rpc_client.clone()));
        Self {
            rpc_client,
            wallet,
            config,
            sender,
        }
    }

    /// Replace the transaction sender (e.g. with a Jito bundle sender).
    pub fn with_sender(mut self, sender: Arc<dyn TxSender>) -> Self {
        self.sender = sender;
        self
    }

    /// Wallet public key.
    pub fn wallet(&self) -> Pubkey {
        self.wallet.pubkey()
    }

    /// v3 trades require the user's token account to exist. Idempotent, so
    /// it never fails if the ATA exists and saves an RPC lookup.
    fn create_ata_instruction(&self, pool: &Pool) -> Instruction {
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &self.wallet.pubkey(),
            &self.wallet.pubkey(),
            &pool.token_mint,
            &pool.token_program(),
        )
    }

    /// Actual token balance held after a fill (falls back to `fallback`).
    async fn token_balance(&self, pool: &Pool, fallback: u64) -> u64 {
        let ata = spl_associated_token_account::get_associated_token_address_with_program_id(
            &self.wallet.pubkey(),
            &pool.token_mint,
            &pool.token_program(),
        );
        match self.rpc_client.get_token_account_balance(&ata).await {
            Ok(b) => b.amount.parse().unwrap_or(fallback),
            Err(e) => {
                warn!(error = %e, "Could not read token balance, using quoted amount");
                fallback
            }
        }
    }

    async fn build_tx(&self, mut instructions: Vec<Instruction>) -> Result<Transaction> {
        instructions.extend(self.sender.extra_instructions(&self.wallet.pubkey()));
        let blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .await
            .map_err(|e| Error::Rpc(format!("Failed to get blockhash: {}", e)))?;
        let mut b = TransactionBuilder::new(self.config.clone())
            .fee_payer(self.wallet.pubkey())
            .instructions(instructions)
            .signer(self.wallet.clone());
        if self.sender.skip_priority_fee() {
            b = b.without_priority_fee();
        }
        b.build(blockhash)
    }

    fn failed(start: Instant, signature: Signature, err: String) -> ExecutionResult {
        ExecutionResult {
            signature,
            success: false,
            tokens_received: None,
            sol_spent_lamports: 0,
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some(err),
            confirmed_slot: None,
        }
    }

    /// Simulate (if configured), send, and poll for confirmation.
    async fn submit(
        &self,
        tx: &Transaction,
        start: Instant,
    ) -> std::result::Result<(Signature, u64), (Signature, String)> {
        if self.config.simulate_first {
            match self.rpc_client.simulate_transaction(tx).await {
                Ok(sim) => {
                    if let Some(err) = sim.value.err {
                        error!(error = ?err, logs = ?sim.value.logs, "Simulation failed");
                        return Err((
                            Signature::default(),
                            format!("Simulation failed: {:?}", err),
                        ));
                    }
                    debug!(units = sim.value.units_consumed, "Simulation ok");
                }
                Err(e) => warn!(error = %e, "Simulation request failed, sending anyway"),
            }
        }

        let signature = match self.sender.send(tx).await {
            Ok(s) => s,
            Err(e) => return Err((Signature::default(), format!("Send failed: {}", e))),
        };
        info!(signature = %signature, sender = self.sender.name(), "Transaction sent");

        // Poll every 500ms for ~ max_retries * 2 seconds.
        let attempts = self.config.max_retries.max(1) * 4;
        for _ in 0..attempts {
            match self.rpc_client.get_signature_statuses(&[signature]).await {
                Ok(resp) => {
                    if let Some(Some(status)) = resp.value.first() {
                        if let Some(err) = &status.err {
                            return Err((signature, format!("Transaction error: {:?}", err)));
                        }
                        if status.satisfies_commitment(self.rpc_client.commitment()) {
                            info!(
                                signature = %signature,
                                slot = status.slot,
                                latency_ms = start.elapsed().as_millis() as u64,
                                "Transaction confirmed"
                            );
                            return Ok((signature, status.slot));
                        }
                    }
                }
                Err(e) => warn!(error = %e, "get_signature_statuses failed"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err((signature, "Confirmation timeout".to_string()))
    }

    /// Fetch the curve, or fall back to launch reserves if it isn't visible yet.
    async fn curve_or_initial(&self, pool: &Pool) -> BondingCurveState {
        match self.curve_state(pool).await {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "Curve not readable, assuming fresh launch reserves");
                BondingCurveState::initial()
            }
        }
    }
}

#[async_trait::async_trait]
impl Executor for PumpFunExecutor {
    async fn execute_buy(
        &self,
        pool: &Pool,
        amount_sol: f64,
        slippage: f64,
    ) -> Result<ExecutionResult> {
        let start = Instant::now();
        let curve = self.curve_or_initial(pool).await;
        if let Err(e) = check_tradeable(&curve) {
            return Ok(Self::failed(start, Signature::default(), e.to_string()));
        }
        let (quote_in, expected, min_tokens_out) = buy_params(&curve, amount_sol, slippage);
        info!(
            mint = %pool.token_mint,
            quote_in,
            expected_tokens = expected,
            min_tokens_out,
            "Executing pump.fun buy"
        );

        let ixs = vec![
            self.create_ata_instruction(pool),
            build_buy_instruction(
                &self.wallet.pubkey(),
                pool,
                quote_in,
                min_tokens_out,
                random_buyback_recipient(),
            )?,
        ];
        let tx = self.build_tx(ixs).await?;

        Ok(match self.submit(&tx, start).await {
            Ok((signature, slot)) => {
                // Sell what we actually hold, not the quote.
                let tokens = self.token_balance(pool, min_tokens_out).await;
                ExecutionResult {
                    signature,
                    success: true,
                    tokens_received: Some(tokens),
                    sol_spent_lamports: quote_in,
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: None,
                    confirmed_slot: Some(slot),
                }
            }
            Err((sig, e)) => Self::failed(start, sig, e),
        })
    }

    async fn execute_sell(
        &self,
        pool: &Pool,
        token_amount: u64,
        slippage: f64,
    ) -> Result<ExecutionResult> {
        let start = Instant::now();
        let curve = self.curve_state(pool).await?;
        check_tradeable(&curve)?;
        let min_sol_output = sell_params(&curve, token_amount, slippage);
        info!(
            mint = %pool.token_mint,
            token_amount,
            min_sol_output,
            "Executing pump.fun sell"
        );

        let ixs = vec![build_sell_instruction(
            &self.wallet.pubkey(),
            pool,
            token_amount,
            min_sol_output,
            random_buyback_recipient(),
        )?];
        let tx = self.build_tx(ixs).await?;

        Ok(match self.submit(&tx, start).await {
            Ok((signature, slot)) => ExecutionResult {
                signature,
                success: true,
                tokens_received: None,
                // For sells this carries the minimum SOL received.
                sol_spent_lamports: min_sol_output,
                latency_ms: start.elapsed().as_millis() as u64,
                error: None,
                confirmed_slot: Some(slot),
            },
            Err((sig, e)) => Self::failed(start, sig, e),
        })
    }

    async fn curve_state(&self, pool: &Pool) -> Result<BondingCurveState> {
        let curve = pool
            .bonding_curve
            .ok_or_else(|| Error::Execution("No bonding curve address".to_string()))?;
        let account = self
            .rpc_client
            .get_account(&curve)
            .await
            .map_err(|e| Error::Rpc(format!("Failed to fetch bonding curve: {}", e)))?;
        BondingCurveState::from_account_data(&account.data)
    }

    async fn simulate(&self, pool: &Pool, amount_sol: f64, slippage: f64) -> Result<()> {
        let curve = self.curve_or_initial(pool).await;
        check_tradeable(&curve)?;
        let (quote_in, _, min_tokens_out) = buy_params(&curve, amount_sol, slippage);
        let ixs = vec![
            self.create_ata_instruction(pool),
            build_buy_instruction(
                &self.wallet.pubkey(),
                pool,
                quote_in,
                min_tokens_out,
                random_buyback_recipient(),
            )?,
        ];
        let tx = self.build_tx(ixs).await?;
        let result = self
            .rpc_client
            .simulate_transaction(&tx)
            .await
            .map_err(|e| Error::Execution(format!("Simulation failed: {}", e)))?;
        if let Some(err) = result.value.err {
            return Err(Error::Execution(format!("Simulation error: {:?}", err)));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "pump.fun"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sniper_core::PoolType;

    fn pool_for(mint: Pubkey, token_program: Option<Pubkey>) -> Pool {
        let curve = Pubkey::find_program_address(
            &[b"bonding-curve", mint.as_ref()],
            &programs::PUMP_FUN_PROGRAM,
        )
        .0;
        Pool {
            address: curve,
            token_mint: mint,
            quote_mint: *programs::WSOL_MINT,
            pool_type: PoolType::PumpFun,
            initial_liquidity_lamports: 0,
            token_reserve: 0,
            creation_slot: 0,
            created_at: Utc::now(),
            creator: None,
            bonding_curve: Some(curve),
            token_program,
            is_mayhem_mode: false,
        }
    }

    /// Every account we derive must equal the one a real mainnet `sell_v3`
    /// used (tx 2eyyahVtB6SP8dLgjvmePYXo5H9xCgmW2DamMJ4Lo73uf3QMRtuLGmjd4sZnzgWp9PXyMZ7QgDA8o43WTw9yF4LW).
    #[test]
    fn matches_mainnet_sell_v3_accounts() {
        let mainnet = [
            "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf",
            "BpDR9CZtZKYMu1a9L8yBfdXVJx9NcbZjkaVYSRq8pump",
            "So11111111111111111111111111111111111111112",
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "FjzSWshoB8msrGDpYnREYKYSpxoreQXTy5WvSCu8wq84",
            "y5qkX918m2HwAjYmmbWqnWwmr6LwYsxVKe9PZeXiSW5",
            "CNtSrKSJCXg4V44CznfVnH33kopqa5GVLA5PwFRQ1xED",
            "6irYUcN28zJ1ZjagjpeQPmE9KmA7hWikebFaEjtEpxT1",
            "AuKqLXH33yEJ41YEVrkCHrvqKg7HC3VVHuRrTNR2V9aU",
            "HqsndV9VqHuHVd8o8jbSm6znJYyVhpVm4gtFQnZwYRiz",
            "ATJHysGRKBVwu1QUnZPiD1Q9aqQKn5VKabvbKuuzPfLA",
            "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt",
            "9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7",
            "11111111111111111111111111111111",
            "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1",
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P",
        ];
        let user = pk(mainnet[8]);
        let p = pool_for(pk(mainnet[1]), Some(*programs::TOKEN_2022_PROGRAM));
        let ix = build_sell_instruction(&user, &p, 1, 0, pk(mainnet[13])).unwrap();
        assert_eq!(ix.accounts.len(), 17);
        for (i, (got, want)) in ix.accounts.iter().zip(mainnet).enumerate() {
            assert_eq!(got.pubkey, pk(want), "account {}", i);
        }
        // Writability per the IDL.
        let writable: Vec<usize> = ix.accounts.iter().enumerate().filter(|(_, a)| a.is_writable).map(|(i, _)| i).collect();
        assert_eq!(writable, vec![5, 6, 7, 8, 9, 10, 11, 13]);
        assert!(ix.accounts[8].is_signer);
        // Data layout matches the mainnet sell_v3 instruction bytes.
        let real = "1c92de7726c469d57f66774c5b0100000000000000000000";
        let amount = u64::from_le_bytes(hex_bytes(real)[8..16].try_into().unwrap());
        let ix = build_sell_instruction(&user, &p, amount, 0, pk(mainnet[13])).unwrap();
        assert_eq!(ix.data, hex_bytes(real));
    }

    fn hex_bytes(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn buy_instruction_data() {
        let user = Pubkey::new_unique();
        let p = pool_for(Pubkey::new_unique(), None);
        let ix = build_buy_instruction(&user, &p, 123, 456, random_buyback_recipient()).unwrap();
        assert_eq!(ix.program_id, *programs::PUMP_FUN_PROGRAM);
        assert_eq!(&ix.data[..8], &discriminators::BUY_EXACT_QUOTE_IN_V3);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 123);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 456);
        assert_eq!(&ix.data[24..], &[1, 0]);
        // Legacy SPL coins use the legacy token program for base accounts.
        assert_eq!(ix.accounts[3].pubkey, *programs::TOKEN_PROGRAM);
        assert!(BUYBACK_FEE_RECIPIENTS.iter().any(|r| pk(r) == ix.accounts[13].pubkey));
    }

    #[test]
    fn missing_curve_is_an_error() {
        let mut p = pool_for(Pubkey::new_unique(), None);
        p.bonding_curve = None;
        assert!(build_buy_instruction(&Pubkey::new_unique(), &p, 1, 1, Pubkey::new_unique()).is_err());
    }

    #[test]
    fn buy_params_use_curve() {
        let c = BondingCurveState::initial();
        let (quote_in, expected, min_out) = buy_params(&c, 1.0, 0.1);
        assert_eq!(quote_in, 1_000_000_000);
        assert!(expected > 34_000_000_000_000 && expected < 34_612_903_225_806, "{}", expected);
        assert_eq!(min_out, curve::with_slippage_down(expected, 0.1));
    }

    #[test]
    fn sell_params_apply_slippage() {
        let mut c = BondingCurveState::initial();
        c.real_sol_reserves = 10_000_000_000;
        let quote = c.sell_quote(1_000_000_000_000, curve::DEFAULT_FEE_BPS);
        assert_eq!(sell_params(&c, 1_000_000_000_000, 0.0), quote);
        assert!(sell_params(&c, 1_000_000_000_000, 0.2) < quote);
    }

    #[test]
    fn refuses_untradeable_curves() {
        let ok = BondingCurveState::initial();
        assert!(check_tradeable(&ok).is_ok());
        let mut c = ok;
        c.complete = true;
        assert!(check_tradeable(&c).is_err());
        let mut c = ok;
        c.quote_mint = [9; 32];
        assert!(check_tradeable(&c).is_err());
        let mut c = ok;
        c.is_cashback_coin = true;
        assert!(check_tradeable(&c).is_err());
    }
}
