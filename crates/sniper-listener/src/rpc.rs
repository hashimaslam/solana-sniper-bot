//! RPC WebSocket listener implementation.

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

use sniper_core::{config::RpcConfig, programs, ParsedTransaction, Result};

use crate::traits::{Listener, TransactionEvent};

/// RPC WebSocket listener for transaction streaming.
pub struct RpcListener {
    config: RpcConfig,
    running: Arc<AtomicBool>,
    /// Programs to filter for
    program_filters: Vec<Pubkey>,
}

impl RpcListener {
    /// Create a new RPC listener with the given configuration.
    pub fn new(config: RpcConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            program_filters: vec![
                *programs::PUMP_FUN_PROGRAM,
                *programs::RAYDIUM_V4_PROGRAM,
                *programs::RAYDIUM_CPMM_PROGRAM,
            ],
        }
    }

    /// Add a program to filter for.
    pub fn with_program_filter(mut self, program: Pubkey) -> Self {
        self.program_filters.push(program);
        self
    }

    /// Get the WebSocket URL from config.
    fn ws_url(&self) -> String {
        if let Some(ref ws) = self.config.ws_endpoint {
            ws.clone()
        } else {
            // Convert HTTP to WS
            self.config
                .endpoint
                .replace("https://", "wss://")
                .replace("http://", "ws://")
        }
    }
}

#[async_trait]
impl Listener for RpcListener {
    async fn start(&self) -> Result<mpsc::Receiver<TransactionEvent>> {
        let (tx, rx) = mpsc::channel(1000);
        let ws_url = self.ws_url();
        let running = self.running.clone();
        let program_filters = self.program_filters.clone();

        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            while running.load(Ordering::SeqCst) {
                info!(url = %ws_url, "Connecting to RPC WebSocket");

                match connect_async(&ws_url).await {
                    Ok((mut ws_stream, _)) => {
                        info!("WebSocket connected");
                        let _ = tx.send(TransactionEvent::Connected).await;

                        // Subscribe to logs for each program
                        for (i, program) in program_filters.iter().enumerate() {
                            let subscribe_msg = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": i + 1,
                                "method": "logsSubscribe",
                                "params": [
                                    {"mentions": [program.to_string()]},
                                    {"commitment": "confirmed"}
                                ]
                            });

                            let msg_text: String = subscribe_msg.to_string();
                            if let Err(e) = ws_stream
                                .send(Message::Text(msg_text.into()))
                                .await
                            {
                                error!(error = %e, "Failed to subscribe");
                                break;
                            }
                            debug!(program = %program, "Subscribed to program logs");
                        }

                        // Process incoming messages
                        while let Some(msg_result) = ws_stream.next().await {
                            if !running.load(Ordering::SeqCst) {
                                break;
                            }

                            match msg_result {
                                Ok(Message::Text(text)) => {
                                    // Try to parse as logs notification
                                    if let Ok(notification) =
                                        serde_json::from_str::<LogsNotificationWrapper>(&text)
                                    {
                                        if notification.method == "logsNotification" {
                                            if let Some(params) = notification.params {
                                                // Extract program IDs from logs
                                                let program_ids_extracted: Vec<Pubkey> = params
                                                    .result
                                                    .value
                                                    .logs
                                                    .iter()
                                                    .filter_map(|log| {
                                                        if log.starts_with("Program ") && log.contains(" invoke") {
                                                            let parts: Vec<&str> = log.split_whitespace().collect();
                                                            if parts.len() >= 2 {
                                                                Pubkey::from_str(parts[1]).ok()
                                                            } else {
                                                                None
                                                            }
                                                        } else {
                                                            None
                                                        }
                                                    })
                                                    .collect();

                                                // Check if any target programs
                                                let has_target = program_ids_extracted
                                                    .iter()
                                                    .any(|p| program_filters.contains(p));

                                                if has_target {
                                                    if let Ok(signature) = Signature::from_str(&params.result.value.signature) {
                                                        let parsed = ParsedTransaction {
                                                            signature,
                                                            slot: params.result.context.slot,
                                                            block_time: None,
                                                            data: vec![],
                                                            account_keys: vec![],
                                                            program_ids: program_ids_extracted,
                                                            success: params.result.value.err.is_none(),
                                                        };

                                                        if tx
                                                            .send(TransactionEvent::Transaction(parsed))
                                                            .await
                                                            .is_err()
                                                        {
                                                            warn!("Channel closed, stopping listener");
                                                            running.store(false, Ordering::SeqCst);
                                                            break;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                Ok(Message::Ping(data)) => {
                                    let _ = ws_stream.send(Message::Pong(data)).await;
                                }
                                Ok(Message::Close(_)) => {
                                    warn!("WebSocket closed by server");
                                    break;
                                }
                                Err(e) => {
                                    error!(error = %e, "WebSocket error");
                                    break;
                                }
                                _ => {}
                            }
                        }

                        let _ = tx
                            .send(TransactionEvent::Disconnected("Connection lost".to_string()))
                            .await;
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to connect to WebSocket");
                        let _ = tx
                            .send(TransactionEvent::Disconnected(format!(
                                "Connection failed: {}",
                                e
                            )))
                            .await;
                    }
                }

                if running.load(Ordering::SeqCst) {
                    warn!("Reconnecting in 5 seconds...");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }

            let _ = tx.send(TransactionEvent::Stopped).await;
            info!("RPC listener stopped");
        });

        Ok(rx)
    }

    async fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn name(&self) -> &'static str {
        "RPC WebSocket"
    }
}

// JSON-RPC types for WebSocket messages

#[derive(Debug, Deserialize)]
struct LogsNotificationWrapper {
    method: String,
    params: Option<LogsNotificationParams>,
}

#[derive(Debug, Deserialize)]
struct LogsNotificationParams {
    result: LogsResult,
}

#[derive(Debug, Deserialize)]
struct LogsResult {
    context: Context,
    value: LogsValue,
}

#[derive(Debug, Deserialize)]
struct Context {
    slot: u64,
}

#[derive(Debug, Deserialize)]
struct LogsValue {
    signature: String,
    err: Option<serde_json::Value>,
    logs: Vec<String>,
}

