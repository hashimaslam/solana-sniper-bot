//! pump.fun executor implementation.
//!
//! Builds and executes buy transactions against pump.fun bonding curves.

use borsh::BorshSerialize;
use chrono::Utc;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    system_program,
};
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, error, info, warn};

use sniper_core::{config::ExecutionConfig, programs, Error, ExecutionResult, Pool, Result};

use crate::transaction::{Executor, TransactionBuilder};

/// pump.fun buy instruction arguments.
#[derive(BorshSerialize)]
struct BuyArgs {
    amount: u64,
    max_sol_cost: u64,
}

/// pump.fun executor for buying tokens.
pub struct PumpFunExecutor {
    rpc_client: Arc<RpcClient>,
    wallet: Arc<Keypair>,
    config: ExecutionConfig,
}

impl PumpFunExecutor {
    /// Create a new pump.fun executor.
    pub fn new(rpc_client: Arc<RpcClient>, wallet: Arc<Keypair>, config: ExecutionConfig) -> Self {
        Self {
            rpc_client,
            wallet,
            config,
        }
    }

    /// Build the buy instruction for pump.fun.
    fn build_buy_instruction(
        &self,
        pool: &Pool,
        token_amount: u64,
        max_sol_cost: u64,
    ) -> Result<Instruction> {
        let bonding_curve = pool
            .bonding_curve
            .ok_or_else(|| Error::Execution("No bonding curve address".to_string()))?;

        // Derive associated token accounts
        let user_ata = spl_associated_token_account::get_associated_token_address(
            &self.wallet.pubkey(),
            &pool.token_mint,
        );

        let bonding_curve_ata = spl_associated_token_account::get_associated_token_address(
            &bonding_curve,
            &pool.token_mint,
        );

        // pump.fun global state PDA
        let (global, _) =
            Pubkey::find_program_address(&[b"global"], &programs::PUMP_FUN_PROGRAM);

        // Fee recipient (pump.fun treasury)
        let fee_recipient =
            Pubkey::try_from("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM").unwrap();

        // Event authority PDA
        let (event_authority, _) =
            Pubkey::find_program_address(&[b"__event_authority"], &programs::PUMP_FUN_PROGRAM);

        // Build instruction data
        let discriminator: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea]; // BUY
        let args = BuyArgs {
            amount: token_amount,
            max_sol_cost,
        };

        let mut data = discriminator.to_vec();
        args.serialize(&mut data)
            .map_err(|e| Error::Serialization(format!("Failed to serialize buy args: {}", e)))?;

        // Account metas for buy instruction
        let accounts = vec![
            AccountMeta::new_readonly(global, false),
            AccountMeta::new(fee_recipient, false),
            AccountMeta::new_readonly(pool.token_mint, false),
            AccountMeta::new(bonding_curve, false),
            AccountMeta::new(bonding_curve_ata, false),
            AccountMeta::new(user_ata, false),
            AccountMeta::new(self.wallet.pubkey(), true),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(spl_token::id(), false),
            AccountMeta::new_readonly(solana_sdk::sysvar::rent::id(), false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(*programs::PUMP_FUN_PROGRAM, false),
        ];

        Ok(Instruction {
            program_id: *programs::PUMP_FUN_PROGRAM,
            accounts,
            data,
        })
    }

    /// Create associated token account if it doesn't exist.
    fn build_create_ata_instruction(&self, mint: &Pubkey) -> Instruction {
        spl_associated_token_account::instruction::create_associated_token_account(
            &self.wallet.pubkey(),
            &self.wallet.pubkey(),
            mint,
            &spl_token::id(),
        )
    }

    /// Check if ATA exists.
    async fn ata_exists(&self, mint: &Pubkey) -> bool {
        let ata = spl_associated_token_account::get_associated_token_address(
            &self.wallet.pubkey(),
            mint,
        );

        self.rpc_client.get_account(&ata).await.is_ok()
    }

    /// Calculate token amount from SOL with slippage.
    fn calculate_buy_params(&self, pool: &Pool, sol_amount: f64, slippage: f64) -> (u64, u64) {
        let sol_lamports = (sol_amount * 1_000_000_000.0) as u64;

        // For pump.fun bonding curve:
        // Price increases as more tokens are bought
        // We estimate based on virtual reserves
        // token_out = (virtual_token_reserves * sol_in) / (virtual_sol_reserves + sol_in)

        // Since we may not have exact reserves, use a conservative estimate
        // Assume 1 SOL gets roughly 1M tokens initially
        let estimated_tokens = (sol_amount * 1_000_000.0) as u64;

        // Apply slippage to max SOL cost
        let max_sol_cost = (sol_lamports as f64 * (1.0 + slippage)) as u64;

        debug!(
            sol_lamports,
            estimated_tokens,
            max_sol_cost,
            slippage,
            "Calculated buy params"
        );

        (estimated_tokens, max_sol_cost)
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

        info!(
            pool = %pool.address,
            mint = %pool.token_mint,
            amount_sol,
            slippage,
            "Executing pump.fun buy"
        );

        // Calculate buy parameters
        let (token_amount, max_sol_cost) = self.calculate_buy_params(pool, amount_sol, slippage);

        // Build instructions
        let mut instructions = Vec::new();

        // Create ATA if needed
        if !self.ata_exists(&pool.token_mint).await {
            debug!("Creating associated token account");
            instructions.push(self.build_create_ata_instruction(&pool.token_mint));
        }

        // Add buy instruction
        instructions.push(self.build_buy_instruction(pool, token_amount, max_sol_cost)?);

        // Get recent blockhash
        let blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .await
            .map_err(|e| Error::Rpc(format!("Failed to get blockhash: {}", e)))?;

        // Build transaction
        let tx = TransactionBuilder::new(self.config.clone())
            .fee_payer(self.wallet.pubkey())
            .instructions(instructions)
            .signer(self.wallet.clone())
            .build(blockhash)?;

        // Simulate if configured
        if self.config.simulate_first {
            match self.rpc_client.simulate_transaction(&tx).await {
                Ok(sim_result) => {
                    if let Some(err) = sim_result.value.err {
                        error!(error = ?err, "Simulation failed");
                        return Ok(ExecutionResult {
                            signature: Signature::default(),
                            success: false,
                            tokens_received: None,
                            sol_spent_lamports: 0,
                            latency_ms: start.elapsed().as_millis() as u64,
                            error: Some(format!("Simulation failed: {:?}", err)),
                            confirmed_slot: None,
                        });
                    }
                    debug!(
                        units = sim_result.value.units_consumed,
                        "Simulation successful"
                    );
                }
                Err(e) => {
                    warn!(error = %e, "Simulation request failed");
                }
            }
        }

        // Send transaction
        let signature = match self.rpc_client.send_transaction(&tx).await {
            Ok(sig) => sig,
            Err(e) => {
                error!(error = %e, "Failed to send transaction");
                return Ok(ExecutionResult {
                    signature: Signature::default(),
                    success: false,
                    tokens_received: None,
                    sol_spent_lamports: 0,
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("Send failed: {}", e)),
                    confirmed_slot: None,
                });
            }
        };

        info!(signature = %signature, "Transaction sent");

        // Confirm transaction
        let mut retries = 0;
        let max_retries = self.config.max_retries;
        let mut confirmed_slot = None;

        while retries < max_retries {
            match self
                .rpc_client
                .get_signature_status(&signature)
                .await
            {
                Ok(Some(status)) => {
                    if let Err(e) = status {
                        error!(error = ?e, "Transaction failed");
                        return Ok(ExecutionResult {
                            signature,
                            success: false,
                            tokens_received: None,
                            sol_spent_lamports: max_sol_cost,
                            latency_ms: start.elapsed().as_millis() as u64,
                            error: Some(format!("Transaction error: {:?}", e)),
                            confirmed_slot: None,
                        });
                    }

                    // Get slot from transaction
                    if let Ok(tx_response) = self.rpc_client.get_transaction(
                        &signature,
                        solana_transaction_status::UiTransactionEncoding::Json,
                    ).await {
                        confirmed_slot = Some(tx_response.slot);
                    }

                    info!(
                        signature = %signature,
                        latency_ms = start.elapsed().as_millis(),
                        "Transaction confirmed"
                    );

                    return Ok(ExecutionResult {
                        signature,
                        success: true,
                        tokens_received: Some(token_amount),
                        sol_spent_lamports: max_sol_cost,
                        latency_ms: start.elapsed().as_millis() as u64,
                        error: None,
                        confirmed_slot,
                    });
                }
                Ok(None) => {
                    debug!(retries, "Transaction not yet confirmed");
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    retries += 1;
                }
                Err(e) => {
                    warn!(error = %e, "Failed to get signature status");
                    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    retries += 1;
                }
            }
        }

        warn!(signature = %signature, "Transaction confirmation timeout");
        Ok(ExecutionResult {
            signature,
            success: false,
            tokens_received: None,
            sol_spent_lamports: 0,
            latency_ms: start.elapsed().as_millis() as u64,
            error: Some("Confirmation timeout".to_string()),
            confirmed_slot: None,
        })
    }

    async fn simulate(
        &self,
        pool: &Pool,
        amount_sol: f64,
        slippage: f64,
    ) -> Result<()> {
        let (token_amount, max_sol_cost) = self.calculate_buy_params(pool, amount_sol, slippage);

        let mut instructions = Vec::new();

        if !self.ata_exists(&pool.token_mint).await {
            instructions.push(self.build_create_ata_instruction(&pool.token_mint));
        }

        instructions.push(self.build_buy_instruction(pool, token_amount, max_sol_cost)?);

        let blockhash = self
            .rpc_client
            .get_latest_blockhash()
            .await
            .map_err(|e| Error::Rpc(format!("Failed to get blockhash: {}", e)))?;

        let tx = TransactionBuilder::new(self.config.clone())
            .fee_payer(self.wallet.pubkey())
            .instructions(instructions)
            .signer(self.wallet.clone())
            .build(blockhash)?;

        let result = self
            .rpc_client
            .simulate_transaction(&tx)
            .await
            .map_err(|e| Error::Execution(format!("Simulation failed: {}", e)))?;

        if let Some(err) = result.value.err {
            return Err(Error::Execution(format!("Simulation error: {:?}", err)));
        }

        debug!(
            units = result.value.units_consumed,
            "Simulation successful"
        );

        Ok(())
    }

    fn name(&self) -> &'static str {
        "pump.fun"
    }
}
