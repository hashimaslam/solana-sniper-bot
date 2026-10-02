//! Transaction builder utilities.

use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::Keypair,
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
    priority_fee: bool,
}

impl TransactionBuilder {
    /// Create a new transaction builder.
    pub fn new(config: ExecutionConfig) -> Self {
        Self {
            config,
            instructions: Vec::new(),
            signers: Vec::new(),
            fee_payer: None,
            priority_fee: true,
        }
    }

    /// Don't add a compute-unit price instruction (e.g. for Jito bundles,
    /// which are prioritised by tip instead).
    pub fn without_priority_fee(mut self) -> Self {
        self.priority_fee = false;
        self
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

        if self.priority_fee && self.config.priority_fee_microlamports > 0 {
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

    /// Sell `token_amount` raw token units back into the pool.
    async fn execute_sell(
        &self,
        pool: &sniper_core::Pool,
        token_amount: u64,
        slippage: f64,
    ) -> Result<sniper_core::ExecutionResult>;

    /// Current on-chain curve state for the pool (used for pricing).
    async fn curve_state(&self, pool: &sniper_core::Pool) -> Result<sniper_core::BondingCurveState>;

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

#[cfg(test)]
#[allow(deprecated)] // system_instruction is fine for building test fixtures
mod tests {
    use super::*;
    use solana_sdk::{hash::Hash, system_instruction};

    fn ix_programs(tx: &Transaction) -> Vec<Pubkey> {
        tx.message
            .instructions
            .iter()
            .map(|ix| tx.message.account_keys[ix.program_id_index as usize])
            .collect()
    }

    #[test]
    fn adds_compute_budget_and_signs() {
        let kp = Arc::new(Keypair::new());
        let ix = system_instruction::transfer(&kp.pubkey(), &Pubkey::new_unique(), 1);
        let tx = TransactionBuilder::new(ExecutionConfig::default())
            .fee_payer(kp.pubkey())
            .instruction(ix)
            .signer(kp.clone())
            .build(Hash::new_unique())
            .unwrap();
        let progs = ix_programs(&tx);
        assert_eq!(progs.len(), 3); // limit + price + transfer
        assert_eq!(progs[0], solana_sdk::compute_budget::id());
        assert!(tx.verify().is_ok());
    }

    #[test]
    fn can_skip_priority_fee() {
        let kp = Arc::new(Keypair::new());
        let ix = system_instruction::transfer(&kp.pubkey(), &Pubkey::new_unique(), 1);
        let tx = TransactionBuilder::new(ExecutionConfig::default())
            .fee_payer(kp.pubkey())
            .instruction(ix)
            .signer(kp)
            .without_priority_fee()
            .build(Hash::new_unique())
            .unwrap();
        assert_eq!(ix_programs(&tx).len(), 2); // limit + transfer
    }
}
