//! Yellowstone Geyser gRPC listener.
//!
//! Streams full transactions that touch the watched programs straight from a
//! validator plugin. Compared to `logsSubscribe` this removes the follow-up
//! `getTransaction` round-trip: the listener hands the decoder the serialized
//! transaction plus its fully resolved account keys (including address
//! lookup table entries), so decoding is purely local.

use async_trait::async_trait;
use solana_sdk::{
    hash::Hash,
    instruction::CompiledInstruction,
    message::{v0, MessageHeader, VersionedMessage},
    pubkey::Pubkey,
    signature::Signature,
    transaction::VersionedTransaction,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tonic::{metadata::AsciiMetadataValue, service::Interceptor, Request, Status};
use tracing::{debug, error, info, warn};

use sniper_core::{config::GeyserConfig, programs, Error, ParsedTransaction, Result};

use crate::traits::{Listener, TransactionEvent};

/// Generated protobuf bindings.
#[allow(clippy::all, missing_docs)]
pub mod proto {
    pub mod geyser {
        tonic::include_proto!("geyser");
    }
    pub mod solana {
        pub mod storage {
            pub mod confirmed_block {
                tonic::include_proto!("solana.storage.confirmed_block");
            }
        }
    }
}

use proto::geyser::{
    geyser_client::GeyserClient, subscribe_update::UpdateOneof, CommitmentLevel,
    SubscribeRequest, SubscribeRequestFilterTransactions, SubscribeRequestPing,
    SubscribeUpdateTransaction,
};
use proto::solana::storage::confirmed_block as pb;

/// Filter name used in the subscribe request.
const FILTER_NAME: &str = "sniper";

/// Build the subscription request for the given programs.
pub fn subscribe_request(programs: &[Pubkey], commitment: CommitmentLevel) -> SubscribeRequest {
    let mut transactions = HashMap::new();
    transactions.insert(
        FILTER_NAME.to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: programs.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        },
    );
    SubscribeRequest {
        transactions,
        commitment: Some(commitment as i32),
        ..Default::default()
    }
}

/// Map a config string to the proto commitment.
pub fn commitment_from_str(s: &str) -> Result<CommitmentLevel> {
    use solana_sdk::commitment_config::CommitmentLevel as C;
    Ok(match sniper_core::config::parse_commitment(s)? {
        C::Processed => CommitmentLevel::Processed,
        C::Confirmed => CommitmentLevel::Confirmed,
        C::Finalized => CommitmentLevel::Finalized,
    })
}

fn pubkey(bytes: &[u8]) -> Result<Pubkey> {
    Pubkey::try_from(bytes).map_err(|_| Error::Decode(format!("bad pubkey len {}", bytes.len())))
}

fn u8_index(i: u32) -> Result<u8> {
    u8::try_from(i).map_err(|_| Error::Decode(format!("index {} out of range", i)))
}

/// Rebuild a `VersionedTransaction` from its protobuf form.
pub fn versioned_transaction(tx: &pb::Transaction) -> Result<VersionedTransaction> {
    let msg = tx
        .message
        .as_ref()
        .ok_or_else(|| Error::Decode("transaction without message".into()))?;
    let header = msg.header.clone().unwrap_or_default();
    let header = MessageHeader {
        num_required_signatures: u8_index(header.num_required_signatures)?,
        num_readonly_signed_accounts: u8_index(header.num_readonly_signed_accounts)?,
        num_readonly_unsigned_accounts: u8_index(header.num_readonly_unsigned_accounts)?,
    };
    let account_keys = msg
        .account_keys
        .iter()
        .map(|k| pubkey(k))
        .collect::<Result<Vec<_>>>()?;
    let recent_blockhash = Hash::new_from_array(
        msg.recent_blockhash
            .as_slice()
            .try_into()
            .map_err(|_| Error::Decode("bad blockhash".into()))?,
    );
    let instructions = msg
        .instructions
        .iter()
        .map(|ix| {
            Ok(CompiledInstruction {
                program_id_index: u8_index(ix.program_id_index)?,
                accounts: ix.accounts.clone(),
                data: ix.data.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let message = if msg.versioned {
        let address_table_lookups = msg
            .address_table_lookups
            .iter()
            .map(|l| {
                Ok(v0::MessageAddressTableLookup {
                    account_key: pubkey(&l.account_key)?,
                    writable_indexes: l.writable_indexes.clone(),
                    readonly_indexes: l.readonly_indexes.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        VersionedMessage::V0(v0::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
            address_table_lookups,
        })
    } else {
        VersionedMessage::Legacy(solana_sdk::message::Message {
            header,
            account_keys,
            recent_blockhash,
            instructions,
        })
    };

    let signatures = tx
        .signatures
        .iter()
        .map(|s| Signature::try_from(s.as_slice()).map_err(|_| Error::Decode("bad signature".into())))
        .collect::<Result<Vec<_>>>()?;

    Ok(VersionedTransaction {
        signatures,
        message,
    })
}

/// Convert a Geyser transaction update into the pipeline's `ParsedTransaction`.
///
/// `data` holds the bincode-serialized `VersionedTransaction`, and
/// `account_keys` holds static keys followed by loaded writable then loaded
/// readonly addresses — the same order instruction account indexes use.
pub fn to_parsed(update: &SubscribeUpdateTransaction) -> Result<ParsedTransaction> {
    let info = update
        .transaction
        .as_ref()
        .ok_or_else(|| Error::Decode("update without transaction".into()))?;
    let tx = info
        .transaction
        .as_ref()
        .ok_or_else(|| Error::Decode("update without transaction body".into()))?;
    let vtx = versioned_transaction(tx)?;

    let mut account_keys = vtx.message.static_account_keys().to_vec();
    let mut success = true;
    if let Some(meta) = &info.meta {
        success = meta.err.is_none();
        for k in meta
            .loaded_writable_addresses
            .iter()
            .chain(meta.loaded_readonly_addresses.iter())
        {
            account_keys.push(pubkey(k)?);
        }
    }

    let program_ids = vtx
        .message
        .instructions()
        .iter()
        .filter_map(|ix| account_keys.get(ix.program_id_index as usize).copied())
        .collect();

    let signature = Signature::try_from(info.signature.as_slice())
        .map_err(|_| Error::Decode("bad signature".into()))?;
    let data = bincode::serialize(&vtx)
        .map_err(|e| Error::Serialization(format!("serialize tx: {}", e)))?;

    Ok(ParsedTransaction {
        signature,
        slot: update.slot,
        block_time: None,
        data,
        account_keys,
        program_ids,
        success,
    })
}

/// Adds the `x-token` auth header.
#[derive(Clone)]
struct TokenInterceptor(Option<AsciiMetadataValue>);

impl Interceptor for TokenInterceptor {
    fn call(&mut self, mut req: Request<()>) -> std::result::Result<Request<()>, Status> {
        if let Some(t) = &self.0 {
            req.metadata_mut().insert("x-token", t.clone());
        }
        Ok(req)
    }
}

/// Reconnect backoff.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub min: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            min: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }
}

/// Geyser gRPC listener.
pub struct GeyserListener {
    config: GeyserConfig,
    programs: Vec<Pubkey>,
    running: Arc<AtomicBool>,
    backoff: Backoff,
}

impl GeyserListener {
    pub fn new(config: GeyserConfig) -> Self {
        Self {
            config,
            programs: vec![*programs::PUMP_FUN_PROGRAM],
            running: Arc::new(AtomicBool::new(false)),
            backoff: Backoff::default(),
        }
    }

    /// Replace the watched program list.
    pub fn with_programs(mut self, programs: Vec<Pubkey>) -> Self {
        self.programs = programs;
        self
    }

    pub fn with_backoff(mut self, backoff: Backoff) -> Self {
        self.backoff = backoff;
        self
    }
}

/// Outcome of one connection attempt.
enum Session {
    /// Stream ended or errored after connecting; reconnect quickly.
    Dropped(String),
    /// Couldn't connect; back off.
    ConnectFailed(String),
    /// Consumer went away; stop.
    ChannelClosed,
}

async fn connect(cfg: &GeyserConfig) -> std::result::Result<GeyserClient<tonic::service::interceptor::InterceptedService<tonic::transport::Channel, TokenInterceptor>>, String> {
    let mut endpoint = tonic::transport::Endpoint::from_shared(cfg.endpoint.clone())
        .map_err(|e| format!("bad endpoint: {}", e))?
        .connect_timeout(Duration::from_millis(cfg.connect_timeout_ms))
        .tcp_nodelay(true)
        .http2_adaptive_window(true)
        .keep_alive_while_idle(true)
        .http2_keep_alive_interval(Duration::from_secs(10));
    if cfg.tls && cfg.endpoint.starts_with("https") {
        endpoint = endpoint
            .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
            .map_err(|e| format!("tls: {}", e))?;
    }
    let channel = endpoint.connect().await.map_err(|e| format!("connect: {}", e))?;
    let token = match &cfg.token {
        Some(t) => Some(
            t.parse::<AsciiMetadataValue>()
                .map_err(|_| "token is not valid ASCII".to_string())?,
        ),
        None => None,
    };
    Ok(GeyserClient::with_interceptor(channel, TokenInterceptor(token))
        .max_decoding_message_size(64 * 1024 * 1024))
}

async fn run_session(
    cfg: &GeyserConfig,
    request: SubscribeRequest,
    tx: &mpsc::Sender<TransactionEvent>,
    running: &AtomicBool,
) -> Session {
    let mut client = match connect(cfg).await {
        Ok(c) => c,
        Err(e) => return Session::ConnectFailed(e),
    };

    // The request stream stays open so we can answer server pings.
    let (req_tx, req_rx) = mpsc::channel::<SubscribeRequest>(16);
    if req_tx.send(request).await.is_err() {
        return Session::Dropped("request channel closed".into());
    }
    let mut stream = match client
        .subscribe(tokio_stream::wrappers::ReceiverStream::new(req_rx))
        .await
    {
        Ok(r) => r.into_inner(),
        Err(status) => return Session::ConnectFailed(format!("subscribe: {}", status)),
    };

    info!(endpoint = %cfg.endpoint, "Geyser stream subscribed");
    if tx.send(TransactionEvent::Connected).await.is_err() {
        return Session::ChannelClosed;
    }

    while let Some(msg) = stream.next().await {
        if !running.load(Ordering::SeqCst) {
            return Session::Dropped("stopped".into());
        }
        let update = match msg {
            Ok(u) => u,
            Err(status) => return Session::Dropped(format!("stream error: {}", status)),
        };
        match update.update_oneof {
            Some(UpdateOneof::Transaction(t)) => match to_parsed(&t) {
                Ok(parsed) if parsed.success => {
                    if tx.send(TransactionEvent::Transaction(parsed)).await.is_err() {
                        return Session::ChannelClosed;
                    }
                }
                Ok(_) => {}
                Err(e) => warn!(error = %e, "Failed to convert Geyser transaction"),
            },
            Some(UpdateOneof::Ping(_)) => {
                // Some providers drop idle streams unless the client answers.
                let _ = req_tx
                    .send(SubscribeRequest {
                        ping: Some(SubscribeRequestPing { id: 1 }),
                        ..Default::default()
                    })
                    .await;
            }
            _ => {}
        }
    }
    Session::Dropped("stream ended".into())
}

#[async_trait]
impl Listener for GeyserListener {
    async fn start(&self) -> Result<mpsc::Receiver<TransactionEvent>> {
        let commitment = commitment_from_str(&self.config.commitment)?;
        let request = subscribe_request(&self.programs, commitment);
        let (tx, rx) = mpsc::channel(4096);
        let running = self.running.clone();
        let cfg = self.config.clone();
        let backoff = self.backoff;
        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let mut delay = backoff.min;
            while running.load(Ordering::SeqCst) {
                match run_session(&cfg, request.clone(), &tx, &running).await {
                    Session::ChannelClosed => {
                        warn!("Consumer dropped, stopping Geyser listener");
                        break;
                    }
                    Session::Dropped(reason) => {
                        warn!(reason, "Geyser stream dropped");
                        delay = backoff.min;
                        let _ = tx.send(TransactionEvent::Disconnected(reason)).await;
                    }
                    Session::ConnectFailed(reason) => {
                        error!(reason, "Geyser connection failed");
                        let _ = tx.send(TransactionEvent::Disconnected(reason)).await;
                    }
                }
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                debug!(delay_ms = delay.as_millis() as u64, "Reconnecting to Geyser");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(backoff.max);
            }
            running.store(false, Ordering::SeqCst);
            let _ = tx.send(TransactionEvent::Stopped).await;
            info!("Geyser listener stopped");
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
        "Geyser gRPC"
    }
}
