//! Transaction builder utilities.

use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signature},
    signer::Signer,
    transaction::Transaction,
};
use std::sync::Arc;
use tracing::debug;

use sniper_core::{config::ExecutionConfig, Result};

/// Builder for Solana transactions with priority fees and compute limits.
pub struct TransactionBuilder {
    config: ExecutionConfig,
    instructions: Vec<Instruction>,
    signers: Vec<Arc<Keypair>>,
    fee_payer: Option<Pubkey>,
}

impl TransactionBuilder {
    /// Create a new transaction builder.
    pub fn new(config: ExecutionConfig) -> Self {
        Self {
            config,
            instructions: Vec::new(),
            signers: Vec::new(),
            fee_payer: None,
        }
    }

    /// Set the fee payer.
    pub fn fee_payer(mut self, payer: Pubkey) -> Self {
        self.fee_payer = Some(payer);
        self
    }

    /// Add an instruction.
    pub fn instruction(mut self, ix: Instruction) -> Self {
        self.instructions.push(ix);
        self
    }

    /// Add multiple instructions.
    pub fn instructions(mut self, ixs: Vec<Instruction>) -> Self {
        self.instructions.extend(ixs);
        self
    }

    /// Add a signer.
    pub fn signer(mut self, signer: Arc<Keypair>) -> Self {
        self.signers.push(signer);
        self
    }

    /// Build the transaction with compute budget instructions.
    pub fn build(self, recent_blockhash: solana_sdk::hash::Hash) -> Result<Transaction> {
        let mut all_instructions = Vec::new();

        // Add compute budget instructions
        if self.config.compute_unit_limit > 0 {
            all_instructions.push(ComputeBudgetInstruction::set_compute_unit_limit(
                self.config.compute_unit_limit,
            ));
            debug!(limit = self.config.compute_unit_limit, "Set compute unit limit");
        }

        if self.config.priority_fee_microlamports > 0 {
            all_instructions.push(ComputeBudgetInstruction::set_compute_unit_price(
                self.config.priority_fee_microlamports,
            ));
            debug!(
                fee = self.config.priority_fee_microlamports,
                "Set priority fee"
            );
        }

        // Add user instructions
        all_instructions.extend(self.instructions);

        // Build message
        let fee_payer = self.fee_payer.unwrap_or_else(|| {
            self.signers
                .first()
                .map(|s| s.pubkey())
                .unwrap_or_default()
        });

        let message = Message::new(&all_instructions, Some(&fee_payer));

        // Create and sign transaction
        let mut tx = Transaction::new_unsigned(message);
        tx.message.recent_blockhash = recent_blockhash;

        // Collect signer references
        let signer_refs: Vec<&Keypair> = self.signers.iter().map(|s| s.as_ref()).collect();

        if !signer_refs.is_empty() {
            tx.sign(&signer_refs, recent_blockhash);
        }

        Ok(tx)
    }

    /// Get the estimated transaction size.
    pub fn estimated_size(&self) -> usize {
        // Base transaction overhead + instruction data
        let base_size = 64 + 32 + 32 + 4; // Signature + fee payer + blockhash + header
        let instructions_size: usize = self
            .instructions
            .iter()
            .map(|ix| 1 + 32 + ix.accounts.len() + ix.data.len())
            .sum();
        base_size + instructions_size
    }
}

/// Trait for transaction executors.
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    /// Execute a buy transaction.
    async fn execute_buy(
        &self,
        pool: &sniper_core::Pool,
        amount_sol: f64,
        slippage: f64,
    ) -> Result<sniper_core::ExecutionResult>;

    /// Simulate a transaction before execution.
    async fn simulate(
        &self,
        pool: &sniper_core::Pool,
        amount_sol: f64,
        slippage: f64,
    ) -> Result<()>;

    /// Get the executor name.
    fn name(&self) -> &'static str;
}
