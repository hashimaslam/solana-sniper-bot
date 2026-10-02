//! Jito block engine bundle submission.
//!
//! A bundle is a list of up to 5 transactions that execute atomically and in
//! order within one slot. Validators running the Jito client order bundles by
//! tip, so the sniper appends a SOL transfer to one of the Jito tip accounts
//! and drops the compute-unit price (it buys nothing inside a bundle).
//!
//! The JSON-RPC surface used here:
//! - `POST {endpoint}/api/v1/bundles`  `sendBundle([[base64 tx, ...], {"encoding":"base64"}])`
//! - `POST {endpoint}/api/v1/bundles`  `getInflightBundleStatuses([[bundle_id]])`

use async_trait::async_trait;
use base64::Engine;
use rand::seq::SliceRandom;
use serde::Deserialize;
use serde_json::{json, Value};
use solana_sdk::{
    instruction::Instruction, pubkey::Pubkey, signature::Signature, transaction::Transaction,
};
use std::str::FromStr;
use std::time::Duration;
use tracing::{debug, info};

use sniper_core::{Error, Result};

use crate::sender::TxSender;

/// Default mainnet block engine.
pub const DEFAULT_BLOCK_ENGINE: &str = "https://mainnet.block-engine.jito.wtf";

/// Maximum transactions per bundle.
pub const MAX_BUNDLE_TXS: usize = 5;

/// Minimum tip accepted by the block engine (lamports).
pub const MIN_TIP_LAMPORTS: u64 = 1_000;

/// Jito's published mainnet tip accounts. Any one works; picking at random
/// spreads write-lock contention across them.
pub const TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Parsed tip accounts.
pub fn tip_accounts() -> Vec<Pubkey> {
    TIP_ACCOUNTS
        .iter()
        .map(|s| Pubkey::from_str(s).expect("valid tip account"))
        .collect()
}

/// Build a tip transfer to a random Jito tip account.
#[allow(deprecated)] // solana_sdk::system_instruction re-export; fine on SDK 2.x
pub fn tip_instruction(payer: &Pubkey, lamports: u64) -> Instruction {
    let accounts = tip_accounts();
    let to = accounts
        .choose(&mut rand::thread_rng())
        .copied()
        .unwrap_or(accounts[0]);
    solana_sdk::system_instruction::transfer(payer, &to, lamports)
}

/// Build the `sendBundle` JSON-RPC body.
pub fn send_bundle_body(txs: &[Transaction]) -> Result<Value> {
    if txs.is_empty() || txs.len() > MAX_BUNDLE_TXS {
        return Err(Error::Execution(format!(
            "bundle must contain 1..={} transactions, got {}",
            MAX_BUNDLE_TXS,
            txs.len()
        )));
    }
    let encoded = txs
        .iter()
        .map(|tx| {
            bincode::serialize(tx)
                .map(|b| base64::engine::general_purpose::STANDARD.encode(b))
                .map_err(|e| Error::Serialization(format!("bundle tx: {}", e)))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [encoded, {"encoding": "base64"}],
    }))
}

#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcErrorBody>,
}

#[derive(Debug, Deserialize)]
struct RpcErrorBody {
    code: i64,
    message: String,
}

/// Status reported by `getInflightBundleStatuses`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleStatus {
    Pending,
    Landed { slot: u64 },
    Failed,
    Invalid,
}

fn parse_rpc<T: serde::de::DeserializeOwned>(body: &str) -> Result<T> {
    let resp: RpcResponse<T> = serde_json::from_str(body)
        .map_err(|e| Error::Execution(format!("bad block engine response: {} ({})", e, body)))?;
    if let Some(err) = resp.error {
        return Err(Error::Execution(format!(
            "block engine error {}: {}",
            err.code, err.message
        )));
    }
    resp.result
        .ok_or_else(|| Error::Execution("block engine returned no result".to_string()))
}

/// Parse a `sendBundle` response into the bundle id.
pub fn parse_send_bundle_response(body: &str) -> Result<String> {
    parse_rpc::<String>(body)
}

/// Parse a `getInflightBundleStatuses` response for a single bundle.
pub fn parse_inflight_status(body: &str) -> Result<BundleStatus> {
    #[derive(Deserialize)]
    struct Wrapper {
        value: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        status: String,
        landed_slot: Option<u64>,
    }
    let w: Wrapper = parse_rpc(body)?;
    let e = w
        .value
        .into_iter()
        .next()
        .ok_or_else(|| Error::Execution("empty bundle status".to_string()))?;
    Ok(match e.status.as_str() {
        "Landed" => BundleStatus::Landed {
            slot: e.landed_slot.unwrap_or_default(),
        },
        "Failed" => BundleStatus::Failed,
        "Invalid" => BundleStatus::Invalid,
        _ => BundleStatus::Pending,
    })
}

/// Jito block engine client.
pub struct JitoClient {
    http: reqwest::Client,
    bundles_url: String,
    auth_uuid: Option<String>,
}

impl JitoClient {
    pub fn new(endpoint: Option<&str>, auth_uuid: Option<String>) -> Result<Self> {
        let base = endpoint.unwrap_or(DEFAULT_BLOCK_ENGINE).trim_end_matches('/');
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| Error::Execution(format!("http client: {}", e)))?;
        Ok(Self {
            http,
            bundles_url: format!("{}/api/v1/bundles", base),
            auth_uuid,
        })
    }

    async fn post(&self, body: &Value) -> Result<String> {
        let mut req = self.http.post(&self.bundles_url).json(body);
        if let Some(uuid) = &self.auth_uuid {
            req = req.header("x-jito-auth", uuid);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Execution(format!("block engine request: {}", e)))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Execution(format!("block engine body: {}", e)))?;
        if status.as_u16() == 429 {
            return Err(Error::Execution("Jito rate limited (429)".to_string()));
        }
        Ok(text)
    }

    /// Submit a bundle, returning its bundle id.
    pub async fn send_bundle(&self, txs: &[Transaction]) -> Result<String> {
        let text = self.post(&send_bundle_body(txs)?).await?;
        parse_send_bundle_response(&text)
    }

    /// Check an in-flight bundle.
    pub async fn inflight_status(&self, bundle_id: &str) -> Result<BundleStatus> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getInflightBundleStatuses",
            "params": [[bundle_id]],
        });
        parse_inflight_status(&self.post(&body).await?)
    }
}

/// [`TxSender`] that wraps each transaction (with tip) in a single-tx bundle.
pub struct JitoSender {
    client: JitoClient,
    tip_lamports: u64,
}

impl JitoSender {
    pub fn new(client: JitoClient, tip_lamports: u64) -> Self {
        Self {
            client,
            tip_lamports: tip_lamports.max(MIN_TIP_LAMPORTS),
        }
    }
}

#[async_trait]
impl TxSender for JitoSender {
    fn extra_instructions(&self, payer: &Pubkey) -> Vec<Instruction> {
        // Tip goes in the same transaction as the trade, so it only pays
        // if the trade lands.
        vec![tip_instruction(payer, self.tip_lamports)]
    }

    fn skip_priority_fee(&self) -> bool {
        true
    }

    async fn send(&self, tx: &Transaction) -> Result<Signature> {
        let bundle_id = self.client.send_bundle(std::slice::from_ref(tx)).await?;
        info!(bundle_id, tip = self.tip_lamports, "Bundle submitted");
        // Confirmation is tracked by signature in the executor; the bundle id
        // is logged for debugging via the Jito explorer.
        debug!(signature = %tx.signatures[0], "Bundle tx signature");
        Ok(tx.signatures[0])
    }

    fn name(&self) -> &'static str {
        "jito"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{hash::Hash, message::Message, signature::Keypair, signer::Signer};

    fn signed_tx(kp: &Keypair, ixs: &[Instruction]) -> Transaction {
        let msg = Message::new(ixs, Some(&kp.pubkey()));
        let mut tx = Transaction::new_unsigned(msg);
        tx.sign(&[kp], Hash::new_unique());
        tx
    }

    #[test]
    fn tip_goes_to_a_known_account() {
        let payer = Pubkey::new_unique();
        let accounts = tip_accounts();
        for _ in 0..20 {
            let ix = tip_instruction(&payer, 5_000);
            assert_eq!(ix.program_id, solana_sdk::system_program::ID);
            assert_eq!(ix.accounts[0].pubkey, payer);
            assert!(accounts.contains(&ix.accounts[1].pubkey));
            // System transfer: u32 tag 2 + u64 lamports
            assert_eq!(&ix.data[..4], &2u32.to_le_bytes());
            assert_eq!(u64::from_le_bytes(ix.data[4..12].try_into().unwrap()), 5_000);
        }
    }

    #[test]
    fn bundle_body_round_trips() {
        let kp = Keypair::new();
        let tx = signed_tx(&kp, &[tip_instruction(&kp.pubkey(), 1_000)]);
        let body = send_bundle_body(std::slice::from_ref(&tx)).unwrap();
        assert_eq!(body["method"], "sendBundle");
        assert_eq!(body["params"][1]["encoding"], "base64");
        let b64 = body["params"][0][0].as_str().unwrap();
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let back: Transaction = bincode::deserialize(&raw).unwrap();
        assert_eq!(back, tx);
    }

    #[test]
    fn bundle_size_limits() {
        assert!(send_bundle_body(&[]).is_err());
        let kp = Keypair::new();
        let tx = signed_tx(&kp, &[tip_instruction(&kp.pubkey(), 1_000)]);
        let six = vec![tx; 6];
        assert!(send_bundle_body(&six).is_err());
        assert!(send_bundle_body(&six[..5]).is_ok());
    }

    #[test]
    fn parses_send_bundle_responses() {
        let ok = r#"{"jsonrpc":"2.0","result":"2id3YC2jK9G5Wo2phDx4gJVAew8DcY5NAojnVuao8rkxwPYPe8cSwE5GzhEgJA2y8fVjDEo6iR6ykBvDxrTQrtpb","id":1}"#;
        assert!(parse_send_bundle_response(ok).unwrap().starts_with("2id3"));
        let err = r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"bundle contains an already processed transaction"},"id":1}"#;
        let e = parse_send_bundle_response(err).unwrap_err().to_string();
        assert!(e.contains("-32602") && e.contains("already processed"));
        assert!(parse_send_bundle_response("not json").is_err());
    }

    #[test]
    fn parses_inflight_statuses() {
        let landed = r#"{"jsonrpc":"2.0","result":{"context":{"slot":280999028},"value":[{"bundle_id":"b1","status":"Landed","landed_slot":280999027}]},"id":1}"#;
        assert_eq!(
            parse_inflight_status(landed).unwrap(),
            BundleStatus::Landed { slot: 280999027 }
        );
        let pending = r#"{"jsonrpc":"2.0","result":{"context":{"slot":1},"value":[{"bundle_id":"b1","status":"Pending","landed_slot":null}]},"id":1}"#;
        assert_eq!(parse_inflight_status(pending).unwrap(), BundleStatus::Pending);
        let failed = r#"{"jsonrpc":"2.0","result":{"context":{"slot":1},"value":[{"bundle_id":"b1","status":"Failed","landed_slot":null}]},"id":1}"#;
        assert_eq!(parse_inflight_status(failed).unwrap(), BundleStatus::Failed);
    }

    #[test]
    fn sender_enforces_min_tip_and_skips_priority_fee() {
        let s = JitoSender::new(JitoClient::new(None, None).unwrap(), 1);
        assert!(s.skip_priority_fee());
        let ix = &s.extra_instructions(&Pubkey::new_unique())[0];
        assert_eq!(
            u64::from_le_bytes(ix.data[4..12].try_into().unwrap()),
            MIN_TIP_LAMPORTS
        );
    }

    /// End-to-end against a local mock block engine (no network).
    #[tokio::test]
    async fn sends_bundle_to_mock_block_engine() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            let mut req = Vec::new();
            // Read headers + body (Content-Length based).
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                req.extend_from_slice(&buf[..n]);
                let s = String::from_utf8_lossy(&req);
                if let Some(h) = s.find("\r\n\r\n") {
                    let len = s[..h]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if req.len() >= h + 4 + len {
                        break;
                    }
                }
            }
            let body = r#"{"jsonrpc":"2.0","result":"bundle-123","id":1}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&req).to_string()
        });

        let client =
            JitoClient::new(Some(&format!("http://{}", addr)), Some("uuid-1".into())).unwrap();
        let sender = JitoSender::new(client, 10_000);
        let kp = Keypair::new();
        let tx = signed_tx(&kp, &sender.extra_instructions(&kp.pubkey()));
        let sig = sender.send(&tx).await.unwrap();
        assert_eq!(sig, tx.signatures[0]);

        let req = server.await.unwrap();
        assert!(req.starts_with("POST /api/v1/bundles"));
        assert!(req.to_ascii_lowercase().contains("x-jito-auth: uuid-1"));
        assert!(req.contains("\"sendBundle\""));
    }
}
