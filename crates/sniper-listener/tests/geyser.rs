//! Geyser listener tests against an in-process mock Yellowstone server.

use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, Message, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::{Transaction, VersionedTransaction},
};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::{wrappers::TcpListenerStream, Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

use sniper_core::{config::GeyserConfig, programs};
use sniper_listener::geyser::{
    proto::geyser::{
        self as g,
        geyser_server::{Geyser, GeyserServer},
        subscribe_update::UpdateOneof,
        CommitmentLevel, SubscribeRequest, SubscribeUpdate, SubscribeUpdateTransaction,
        SubscribeUpdateTransactionInfo,
    },
    proto::solana::storage::confirmed_block as pb,
    subscribe_request, to_parsed, versioned_transaction, Backoff, GeyserListener,
};
use sniper_listener::{Listener, TransactionEvent};

// ---------- helpers: VersionedTransaction -> protobuf ----------

fn to_pb(vtx: &VersionedTransaction) -> pb::Transaction {
    let (header, keys, bh, ixs, versioned, luts) = match &vtx.message {
        VersionedMessage::Legacy(m) => (
            m.header,
            m.account_keys.clone(),
            m.recent_blockhash,
            m.instructions.clone(),
            false,
            vec![],
        ),
        VersionedMessage::V0(m) => (
            m.header,
            m.account_keys.clone(),
            m.recent_blockhash,
            m.instructions.clone(),
            true,
            m.address_table_lookups.clone(),
        ),
    };
    pb::Transaction {
        signatures: vtx.signatures.iter().map(|s| s.as_ref().to_vec()).collect(),
        message: Some(pb::Message {
            header: Some(pb::MessageHeader {
                num_required_signatures: header.num_required_signatures as u32,
                num_readonly_signed_accounts: header.num_readonly_signed_accounts as u32,
                num_readonly_unsigned_accounts: header.num_readonly_unsigned_accounts as u32,
            }),
            account_keys: keys.iter().map(|k| k.to_bytes().to_vec()).collect(),
            recent_blockhash: bh.to_bytes().to_vec(),
            instructions: ixs
                .iter()
                .map(|i| pb::CompiledInstruction {
                    program_id_index: i.program_id_index as u32,
                    accounts: i.accounts.clone(),
                    data: i.data.clone(),
                })
                .collect(),
            versioned,
            address_table_lookups: luts
                .iter()
                .map(|l| pb::MessageAddressTableLookup {
                    account_key: l.account_key.to_bytes().to_vec(),
                    writable_indexes: l.writable_indexes.clone(),
                    readonly_indexes: l.readonly_indexes.clone(),
                })
                .collect(),
            config: None,
        }),
    }
}

fn update(vtx: &VersionedTransaction, slot: u64, meta: Option<pb::TransactionStatusMeta>) -> SubscribeUpdateTransaction {
    SubscribeUpdateTransaction {
        transaction: Some(SubscribeUpdateTransactionInfo {
            signature: vtx.signatures[0].as_ref().to_vec(),
            is_vote: false,
            transaction: Some(to_pb(vtx)),
            meta,
            index: 0,
        }),
        slot,
        bank_id: 0,
    }
}

fn pump_ix(payer: &Pubkey, extra: Pubkey) -> Instruction {
    Instruction {
        program_id: *programs::PUMP_FUN_PROGRAM,
        accounts: vec![AccountMeta::new(*payer, true), AccountMeta::new(extra, false)],
        data: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
    }
}

fn legacy_tx() -> VersionedTransaction {
    let kp = Keypair::new();
    let msg = Message::new(&[pump_ix(&kp.pubkey(), Pubkey::new_unique())], Some(&kp.pubkey()));
    let mut tx = Transaction::new_unsigned(msg);
    tx.sign(&[&kp], Hash::new_unique());
    tx.into()
}

// ---------- conversion tests ----------

#[test]
fn legacy_roundtrip() {
    let vtx = legacy_tx();
    let back = versioned_transaction(&to_pb(&vtx)).unwrap();
    assert_eq!(back, vtx);
    assert!(back.verify_with_results().iter().all(|ok| *ok));
}

#[test]
fn v0_with_lookup_tables_resolves_loaded_keys() {
    let kp = Keypair::new();
    let lut = Pubkey::new_unique();
    let loaded_w = Pubkey::new_unique();
    let loaded_r = Pubkey::new_unique();
    // Static keys: payer, program. Index 2 = loaded writable, 3 = loaded readonly.
    let msg = v0::Message {
        header: solana_sdk::message::MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 1,
        },
        account_keys: vec![kp.pubkey(), *programs::PUMP_FUN_PROGRAM],
        recent_blockhash: Hash::new_unique(),
        instructions: vec![solana_sdk::instruction::CompiledInstruction {
            program_id_index: 1,
            accounts: vec![0, 2, 3],
            data: vec![9; 8],
        }],
        address_table_lookups: vec![v0::MessageAddressTableLookup {
            account_key: lut,
            writable_indexes: vec![0],
            readonly_indexes: vec![1],
        }],
    };
    let vtx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&kp]).unwrap();
    let meta = pb::TransactionStatusMeta {
        loaded_writable_addresses: vec![loaded_w.to_bytes().to_vec()],
        loaded_readonly_addresses: vec![loaded_r.to_bytes().to_vec()],
        ..Default::default()
    };
    let parsed = to_parsed(&update(&vtx, 42, Some(meta))).unwrap();
    assert_eq!(parsed.slot, 42);
    assert_eq!(parsed.signature, vtx.signatures[0]);
    assert!(parsed.success);
    assert_eq!(
        parsed.account_keys,
        vec![kp.pubkey(), *programs::PUMP_FUN_PROGRAM, loaded_w, loaded_r]
    );
    assert_eq!(parsed.program_ids, vec![*programs::PUMP_FUN_PROGRAM]);
    let decoded: VersionedTransaction = bincode::deserialize(&parsed.data).unwrap();
    assert_eq!(decoded, vtx);
}

#[test]
fn failed_tx_is_flagged() {
    let meta = pb::TransactionStatusMeta {
        err: Some(pb::TransactionError { err: vec![1] }),
        ..Default::default()
    };
    assert!(!to_parsed(&update(&legacy_tx(), 1, Some(meta))).unwrap().success);
}

#[test]
fn malformed_updates_error_instead_of_panicking() {
    let mut u = update(&legacy_tx(), 1, None);
    u.transaction.as_mut().unwrap().signature = vec![1, 2, 3];
    assert!(to_parsed(&u).is_err());

    let mut u = update(&legacy_tx(), 1, None);
    let m = u.transaction.as_mut().unwrap().transaction.as_mut().unwrap().message.as_mut().unwrap();
    m.account_keys[0] = vec![0; 5];
    assert!(to_parsed(&u).is_err());

    let mut u = update(&legacy_tx(), 1, None);
    u.transaction.as_mut().unwrap().transaction = None;
    assert!(to_parsed(&u).is_err());
}

#[test]
fn subscribe_request_filters_programs() {
    let p = *programs::PUMP_FUN_PROGRAM;
    let r = subscribe_request(&[p], CommitmentLevel::Processed);
    let f = &r.transactions["sniper"];
    assert_eq!(f.account_include, vec![p.to_string()]);
    assert_eq!(f.vote, Some(false));
    assert_eq!(f.failed, Some(false));
    assert_eq!(r.commitment, Some(CommitmentLevel::Processed as i32));
}

// ---------- mock server ----------

#[derive(Default)]
struct Seen {
    tokens: Vec<Option<String>>,
    requests: Vec<SubscribeRequest>,
}

struct MockGeyser {
    seen: Arc<Mutex<Seen>>,
    /// Updates to send on each connection; connection `n` gets `script[n]`.
    script: Arc<Vec<Vec<SubscribeUpdate>>>,
    conns: Arc<AtomicUsize>,
}

type UpdateStream = Pin<Box<dyn Stream<Item = Result<SubscribeUpdate, Status>> + Send>>;

#[tonic::async_trait]
impl Geyser for MockGeyser {
    type SubscribeStream = UpdateStream;
    type SubscribeDeshredStream = Pin<Box<dyn Stream<Item = Result<g::SubscribeUpdateDeshred, Status>> + Send>>;
    type SubscribeGossipStream = Pin<Box<dyn Stream<Item = Result<g::SubscribeUpdateGossip, Status>> + Send>>;

    async fn subscribe(
        &self,
        req: Request<Streaming<SubscribeRequest>>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let token = req
            .metadata()
            .get("x-token")
            .map(|v| v.to_str().unwrap().to_string());
        let mut inbound = req.into_inner();
        let first = inbound.next().await.unwrap()?;
        {
            let mut s = self.seen.lock().await;
            s.tokens.push(token);
            s.requests.push(first);
        }
        let n = self.conns.fetch_add(1, Ordering::SeqCst);
        let updates = self.script.get(n).cloned().unwrap_or_default();
        let last = n + 1 >= self.script.len();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            for u in updates {
                let _ = tx.send(Ok(u)).await;
            }
            if last {
                // Keep the final stream open.
                tx.closed().await;
            }
            // Otherwise dropping tx ends the stream -> client reconnects.
        });
        Ok(Response::new(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))))
    }

    async fn subscribe_deshred(&self, _: Request<Streaming<g::SubscribeDeshredRequest>>) -> Result<Response<Self::SubscribeDeshredStream>, Status> {
        Err(Status::unimplemented(""))
    }
    async fn subscribe_gossip(&self, _: Request<g::SubscribeGossipRequest>) -> Result<Response<Self::SubscribeGossipStream>, Status> {
        Err(Status::unimplemented(""))
    }
    async fn subscribe_replay_info(&self, _: Request<g::SubscribeReplayInfoRequest>) -> Result<Response<g::SubscribeReplayInfoResponse>, Status> {
        Err(Status::unimplemented(""))
    }
    async fn ping(&self, _: Request<g::PingRequest>) -> Result<Response<g::PongResponse>, Status> {
        Err(Status::unimplemented(""))
    }
    async fn get_latest_blockhash(&self, _: Request<g::GetLatestBlockhashRequest>) -> Result<Response<g::GetLatestBlockhashResponse>, Status> {
        Err(Status::unimplemented(""))
    }
    async fn get_block_height(&self, _: Request<g::GetBlockHeightRequest>) -> Result<Response<g::GetBlockHeightResponse>, Status> {
        Err(Status::unimplemented(""))
    }
    async fn get_slot(&self, _: Request<g::GetSlotRequest>) -> Result<Response<g::GetSlotResponse>, Status> {
        Err(Status::unimplemented(""))
    }
    async fn is_blockhash_valid(&self, _: Request<g::IsBlockhashValidRequest>) -> Result<Response<g::IsBlockhashValidResponse>, Status> {
        Err(Status::unimplemented(""))
    }
    async fn get_version(&self, _: Request<g::GetVersionRequest>) -> Result<Response<g::GetVersionResponse>, Status> {
        Err(Status::unimplemented(""))
    }
}

fn tx_update(vtx: &VersionedTransaction, slot: u64, failed: bool) -> SubscribeUpdate {
    let meta = pb::TransactionStatusMeta {
        err: failed.then(|| pb::TransactionError { err: vec![1] }),
        ..Default::default()
    };
    SubscribeUpdate {
        filters: vec!["sniper".into()],
        update_oneof: Some(UpdateOneof::Transaction(update(vtx, slot, Some(meta)))),
        created_at: None,
    }
}

fn ping() -> SubscribeUpdate {
    SubscribeUpdate {
        filters: vec![],
        update_oneof: Some(UpdateOneof::Ping(g::SubscribeUpdatePing {})),
        created_at: None,
    }
}

async fn start_mock(script: Vec<Vec<SubscribeUpdate>>) -> (String, Arc<Mutex<Seen>>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let conns = Arc::new(AtomicUsize::new(0));
    let svc = MockGeyser {
        seen: seen.clone(),
        script: Arc::new(script),
        conns: conns.clone(),
    };
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(GeyserServer::new(svc))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    (format!("http://{}", addr), seen, conns)
}

async fn next_tx(rx: &mut mpsc::Receiver<TransactionEvent>) -> sniper_core::ParsedTransaction {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(TransactionEvent::Transaction(t))) => return t,
            Ok(Some(_)) => continue,
            other => panic!("expected transaction, got {:?}", other.map(|o| o.map(|_| ()))),
        }
    }
}

#[tokio::test]
async fn streams_transactions_with_auth_and_reconnects() {
    let a = legacy_tx();
    let failed = legacy_tx();
    let b = legacy_tx();
    let (endpoint, seen, conns) = start_mock(vec![
        // First connection: one good tx, a ping, a failed tx, then drop.
        vec![tx_update(&a, 10, false), ping(), tx_update(&failed, 11, true)],
        // Second connection after reconnect.
        vec![tx_update(&b, 12, false)],
    ])
    .await;

    let listener = GeyserListener::new(GeyserConfig {
        endpoint,
        token: Some("secret-token".into()),
        tls: false,
        commitment: "processed".into(),
        connect_timeout_ms: 2_000,
    })
    .with_backoff(Backoff {
        min: Duration::from_millis(20),
        max: Duration::from_millis(100),
    });

    let mut rx = listener.start().await.unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap(),
        Some(TransactionEvent::Connected)
    ));

    let t1 = next_tx(&mut rx).await;
    assert_eq!(t1.signature, a.signatures[0]);
    assert_eq!(t1.slot, 10);
    assert!(!t1.data.is_empty(), "full tx is delivered, no RPC fetch needed");

    // Failed tx is dropped; next one comes from the reconnected stream.
    let t2 = next_tx(&mut rx).await;
    assert_eq!(t2.signature, b.signatures[0]);
    assert_eq!(t2.slot, 12);
    assert_eq!(conns.load(Ordering::SeqCst), 2);

    let s = seen.lock().await;
    assert_eq!(s.tokens, vec![Some("secret-token".into()); 2]);
    let f = &s.requests[0].transactions["sniper"];
    assert_eq!(f.account_include, vec![programs::PUMP_FUN_PROGRAM.to_string()]);
    assert_eq!(s.requests[0].commitment, Some(CommitmentLevel::Processed as i32));
    drop(s);

    listener.stop().await.unwrap();
    assert!(!listener.is_running());
}

#[tokio::test]
async fn reports_connect_failures_and_keeps_retrying() {
    // Nothing listens on this port.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let listener = GeyserListener::new(GeyserConfig {
        endpoint: format!("http://127.0.0.1:{}", port),
        token: None,
        tls: false,
        commitment: "confirmed".into(),
        connect_timeout_ms: 200,
    })
    .with_backoff(Backoff {
        min: Duration::from_millis(10),
        max: Duration::from_millis(20),
    });
    let mut rx = listener.start().await.unwrap();
    for _ in 0..2 {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap() {
            Some(TransactionEvent::Disconnected(reason)) => assert!(reason.contains("connect")),
            other => panic!("unexpected {:?}", other.map(|_| ())),
        }
    }
    listener.stop().await.unwrap();
    // Listener drains to Stopped after stop().
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap() {
            Some(TransactionEvent::Stopped) | None => break,
            _ => {}
        }
    }
}

#[tokio::test]
async fn rejects_bad_commitment() {
    let listener = GeyserListener::new(GeyserConfig {
        endpoint: "http://127.0.0.1:1".into(),
        token: None,
        tls: false,
        commitment: "instant".into(),
        connect_timeout_ms: 100,
    });
    assert!(listener.start().await.is_err());
}
