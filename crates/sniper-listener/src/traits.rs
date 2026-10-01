//! Listener trait definition.

use async_trait::async_trait;
use sniper_core::{ParsedTransaction, Result};
use tokio::sync::mpsc;

/// Event emitted by the listener.
#[derive(Debug, Clone)]
pub enum TransactionEvent {
    /// New transaction received
    Transaction(ParsedTransaction),
    /// Connection established
    Connected,
    /// Connection lost (will auto-reconnect)
    Disconnected(String),
    /// Listener stopped
    Stopped,
}

/// Trait for transaction stream listeners.
#[async_trait]
pub trait Listener: Send + Sync {
    /// Start listening for transactions.
    /// Returns a channel receiver for transaction events.
    async fn start(&self) -> Result<mpsc::Receiver<TransactionEvent>>;

    /// Stop the listener.
    async fn stop(&self) -> Result<()>;

    /// Check if the listener is currently running.
    fn is_running(&self) -> bool;

    /// Get the listener type name.
    fn name(&self) -> &'static str;
}
