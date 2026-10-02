//! Transaction submission backends.
//!
//! Executors build and sign transactions; a [`TxSender`] decides how they
//! reach the leader. Swapping the sender is how Jito bundles are enabled
//! without touching any pump.fun logic.

use async_trait::async_trait;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::{instruction::Instruction, pubkey::Pubkey, signature::Signature, transaction::Transaction};
use std::sync::Arc;
use tracing::{debug, warn};

use sniper_core::{Error, Result};

/// A way of getting a signed transaction on-chain.
#[async_trait]
pub trait TxSender: Send + Sync {
    /// Instructions that must be appended to every transaction before it is
    /// signed (e.g. a Jito tip transfer). Empty by default.
    fn extra_instructions(&self, _payer: &Pubkey) -> Vec<Instruction> {
        Vec::new()
    }

    /// Whether the executor should skip adding a compute-unit price.
    /// Jito bundles are ordered by tip, so priority fees are wasted there.
    fn skip_priority_fee(&self) -> bool {
        false
    }

    /// Submit the transaction. Returns its signature once accepted.
    async fn send(&self, tx: &Transaction) -> Result<Signature>;

    /// Human readable name for logs.
    fn name(&self) -> &'static str;
}

/// Plain `sendTransaction` over JSON-RPC.
pub struct RpcSender {
    rpc_client: Arc<RpcClient>,
}

impl RpcSender {
    pub fn new(rpc_client: Arc<RpcClient>) -> Self {
        Self { rpc_client }
    }
}

#[async_trait]
impl TxSender for RpcSender {
    async fn send(&self, tx: &Transaction) -> Result<Signature> {
        // We simulate separately (if configured), so skip the node's preflight
        // to save a round-trip on the hot path.
        let cfg = RpcSendTransactionConfig {
            skip_preflight: true,
            max_retries: Some(0),
            ..Default::default()
        };
        let sig = self
            .rpc_client
            .send_transaction_with_config(tx, cfg)
            .await
            .map_err(|e| Error::Execution(format!("RPC send failed: {}", e)))?;
        debug!(signature = %sig, "Sent via RPC");
        Ok(sig)
    }

    fn name(&self) -> &'static str {
        "rpc"
    }
}

/// Sends through a primary sender and, best-effort, through extra senders in
/// parallel. The primary's result decides success.
pub struct RaceSender {
    primary: Arc<dyn TxSender>,
    secondary: Vec<Arc<dyn TxSender>>,
}

impl RaceSender {
    pub fn new(primary: Arc<dyn TxSender>, secondary: Vec<Arc<dyn TxSender>>) -> Self {
        Self { primary, secondary }
    }
}

#[async_trait]
impl TxSender for RaceSender {
    fn extra_instructions(&self, payer: &Pubkey) -> Vec<Instruction> {
        self.primary.extra_instructions(payer)
    }

    fn skip_priority_fee(&self) -> bool {
        self.primary.skip_priority_fee()
    }

    async fn send(&self, tx: &Transaction) -> Result<Signature> {
        for s in &self.secondary {
            let s = s.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                if let Err(e) = s.send(&tx).await {
                    warn!(sender = s.name(), error = %e, "Secondary send failed");
                }
            });
        }
        self.primary.send(tx).await
    }

    fn name(&self) -> &'static str {
        "race"
    }
}
