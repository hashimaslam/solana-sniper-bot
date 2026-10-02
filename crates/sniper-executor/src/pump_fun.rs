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

use borsh::BorshSerialize;
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

/// Instruction discriminators.
pub mod discriminators {
    pub const BUY: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
    pub const SELL: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];
}

/// pump.fun fee recipient (treasury).
pub const FEE_RECIPIENT: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";

#[derive(BorshSerialize)]
struct BuyArgs {
    amount: u64,
    max_sol_cost: u64,
}

#[derive(BorshSerialize)]
struct SellArgs {
    amount: u64,
    min_sol_output: u64,
}

/// Derived accounts shared by buy and sell.
struct TradeAccounts {
    global: Pubkey,
    fee_recipient: Pubkey,
    bonding_curve: Pubkey,
    bonding_curve_ata: Pubkey,
    user_ata: Pubkey,
    event_authority: Pubkey,
}

fn trade_accounts(user: &Pubkey, pool: &Pool) -> Result<TradeAccounts> {
    let bonding_curve = pool
        .bonding_curve
        .ok_or_else(|| Error::Execution("No bonding curve address".to_string()))?;
    let program = &*programs::PUMP_FUN_PROGRAM;
    Ok(TradeAccounts {
        global: Pubkey::find_program_address(&[b"global"], program).0,
        fee_recipient: Pubkey::from_str(FEE_RECIPIENT).expect("valid const pubkey"),
        bonding_curve,
        bonding_curve_ata: spl_associated_token_account::get_associated_token_address(
            &bonding_curve,
            &pool.token_mint,
        ),
        user_ata: spl_associated_token_account::get_associated_token_address(
            user,
            &pool.token_mint,
        ),
        event_authority: Pubkey::find_program_address(&[b"__event_authority"], program).0,
    })
}

/// Build the pump.fun buy instruction. Pure, so it can be unit tested.
pub fn build_buy_instruction(
    user: &Pubkey,
    pool: &Pool,
    token_amount: u64,
    max_sol_cost: u64,
) -> Result<Instruction> {
    let a = trade_accounts(user, pool)?;
    let mut data = discriminators::BUY.to_vec();
    BuyArgs {
        amount: token_amount,
        max_sol_cost,
    }
    .serialize(&mut data)
    .map_err(|e| Error::Serialization(format!("buy args: {}", e)))?;

    Ok(Instruction {
        program_id: *programs::PUMP_FUN_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(a.global, false),
            AccountMeta::new(a.fee_recipient, false),
            AccountMeta::new_readonly(pool.token_mint, false),
            AccountMeta::new(a.bonding_curve, false),
            AccountMeta::new(a.bonding_curve_ata, false),
            AccountMeta::new(a.user_ata, false),
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(*programs::SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(solana_sdk::sysvar::rent::id(), false),
            AccountMeta::new_readonly(a.event_authority, false),
            AccountMeta::new_readonly(*programs::PUMP_FUN_PROGRAM, false),
        ],
        data,
    })
}

/// Build the pump.fun sell instruction. Pure, so it can be unit tested.
pub fn build_sell_instruction(
    user: &Pubkey,
    pool: &Pool,
    token_amount: u64,
    min_sol_output: u64,
) -> Result<Instruction> {
    let a = trade_accounts(user, pool)?;
    let mut data = discriminators::SELL.to_vec();
    SellArgs {
        amount: token_amount,
        min_sol_output,
    }
    .serialize(&mut data)
    .map_err(|e| Error::Serialization(format!("sell args: {}", e)))?;

    Ok(Instruction {
        program_id: *programs::PUMP_FUN_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(a.global, false),
            AccountMeta::new(a.fee_recipient, false),
            AccountMeta::new_readonly(pool.token_mint, false),
            AccountMeta::new(a.bonding_curve, false),
            AccountMeta::new(a.bonding_curve_ata, false),
            AccountMeta::new(a.user_ata, false),
            AccountMeta::new(*user, true),
            AccountMeta::new_readonly(*programs::SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(spl_associated_token_account::id(), false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(a.event_authority, false),
            AccountMeta::new_readonly(*programs::PUMP_FUN_PROGRAM, false),
        ],
        data,
    })
}

/// Compute `(token_amount, max_sol_cost)` for spending `sol_amount` SOL.
pub fn buy_params(curve: &BondingCurveState, sol_amount: f64, slippage: f64) -> (u64, u64) {
    let sol_lamports = (sol_amount * 1e9) as u64;
    let tokens = curve.buy_quote(sol_lamports, curve::DEFAULT_FEE_BPS);
    // Ask for exactly the quoted tokens and allow paying up to +slippage.
    let max_sol_cost = curve::with_slippage_up(sol_lamports, slippage);
    (tokens, max_sol_cost)
}

/// Compute `min_sol_output` for selling `tokens`.
pub fn sell_params(curve: &BondingCurveState, tokens: u64, slippage: f64) -> u64 {
    curve::with_slippage_down(curve.sell_quote(tokens, curve::DEFAULT_FEE_BPS), slippage)
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

    fn create_ata_instruction(&self, mint: &Pubkey) -> Instruction {
        // Idempotent: never fails if the ATA exists, and saves an RPC lookup.
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &self.wallet.pubkey(),
            &self.wallet.pubkey(),
            mint,
            &spl_token::id(),
        )
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
        if curve.complete {
            return Ok(Self::failed(
                start,
                Signature::default(),
                "Bonding curve complete".into(),
            ));
        }
        let (token_amount, max_sol_cost) = buy_params(&curve, amount_sol, slippage);
        info!(
            mint = %pool.token_mint,
            amount_sol,
            token_amount,
            max_sol_cost,
            "Executing pump.fun buy"
        );

        let ixs = vec![
            self.create_ata_instruction(&pool.token_mint),
            build_buy_instruction(&self.wallet.pubkey(), pool, token_amount, max_sol_cost)?,
        ];
        let tx = self.build_tx(ixs).await?;

        Ok(match self.submit(&tx, start).await {
            Ok((signature, slot)) => ExecutionResult {
                signature,
                success: true,
                tokens_received: Some(token_amount),
                sol_spent_lamports: (amount_sol * 1e9) as u64,
                latency_ms: start.elapsed().as_millis() as u64,
                error: None,
                confirmed_slot: Some(slot),
            },
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
        let (token_amount, max_sol_cost) = buy_params(&curve, amount_sol, slippage);
        let ixs = vec![
            self.create_ata_instruction(&pool.token_mint),
            build_buy_instruction(&self.wallet.pubkey(), pool, token_amount, max_sol_cost)?,
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

    fn pool() -> Pool {
        let curve = Pubkey::new_unique();
        Pool {
            address: curve,
            token_mint: Pubkey::new_unique(),
            quote_mint: *programs::WSOL_MINT,
            pool_type: PoolType::PumpFun,
            initial_liquidity_lamports: 0,
            token_reserve: 0,
            creation_slot: 0,
            created_at: Utc::now(),
            creator: None,
            bonding_curve: Some(curve),
        }
    }

    #[test]
    fn buy_instruction_layout() {
        let user = Pubkey::new_unique();
        let p = pool();
        let ix = build_buy_instruction(&user, &p, 123, 456).unwrap();
        assert_eq!(ix.program_id, *programs::PUMP_FUN_PROGRAM);
        assert_eq!(&ix.data[..8], &discriminators::BUY);
        assert_eq!(u64::from_le_bytes(ix.data[8..16].try_into().unwrap()), 123);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 456);
        assert_eq!(ix.accounts.len(), 12);
        assert_eq!(ix.accounts[2].pubkey, p.token_mint);
        assert_eq!(ix.accounts[3].pubkey, p.bonding_curve.unwrap());
        assert!(ix.accounts[6].is_signer && ix.accounts[6].pubkey == user);
    }

    #[test]
    fn sell_instruction_layout() {
        let user = Pubkey::new_unique();
        let p = pool();
        let ix = build_sell_instruction(&user, &p, 1_000, 7).unwrap();
        assert_eq!(&ix.data[..8], &discriminators::SELL);
        assert_eq!(u64::from_le_bytes(ix.data[16..24].try_into().unwrap()), 7);
        assert_eq!(ix.accounts[8].pubkey, spl_associated_token_account::id());
        assert_eq!(ix.accounts[9].pubkey, spl_token::id());
    }

    #[test]
    fn missing_curve_is_an_error() {
        let mut p = pool();
        p.bonding_curve = None;
        assert!(build_buy_instruction(&Pubkey::new_unique(), &p, 1, 1).is_err());
    }

    #[test]
    fn buy_params_use_curve() {
        let c = BondingCurveState::initial();
        let (tokens, max_cost) = buy_params(&c, 1.0, 0.1);
        assert_eq!(max_cost, 1_100_000_000);
        // ~34.3M tokens (6 decimals) after the 1% fee.
        assert!(
            tokens > 34_000_000_000_000 && tokens < 34_612_903_225_806,
            "{}",
            tokens
        );
    }

    #[test]
    fn sell_params_apply_slippage() {
        let mut c = BondingCurveState::initial();
        c.real_sol_reserves = 10_000_000_000;
        let quote = c.sell_quote(1_000_000_000_000, curve::DEFAULT_FEE_BPS);
        assert_eq!(sell_params(&c, 1_000_000_000_000, 0.0), quote);
        assert!(sell_params(&c, 1_000_000_000_000, 0.2) < quote);
    }
}
