//! PESTO13-01: a local derivation/session failure aborts the download at
//! once instead of being classified as provider corruption and retried on
//! the next tier.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pesto::config::ServerEntry;
use pesto::crypto::{DownloadDecryptionAdapter, EncryptionSession, UploadEncryptionAdapter};
use pesto::yenc::PartSpec;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};

mod support;
use penne::cache;
use penne::config::ServerTier;
use penne::download::download_queue_with_decryptor;
use penne::queue::{DownloadQueue, QueuedFile, QueuedSegment};
use support::fixture_kdf_input;

fn spawn_fake_server_counting(
    known: HashMap<String, Vec<u8>>,
    counter: Arc<AtomicUsize>,
) -> SocketAddr {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(handle_connection(stream, known.clone(), counter.clone()));
        }
    });
    addr
}

async fn handle_connection(
    stream: TcpStream,
    known: HashMap<String, Vec<u8>>,
    counter: Arc<AtomicUsize>,
) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    if w.write_all(b"200 mock ready\r\n").await.is_err() {
        return;
    }
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let cmd = line.trim_end();
        if let Some(rest) = cmd.strip_prefix("BODY ") {
            counter.fetch_add(1, Ordering::SeqCst);
            let id = rest.trim_start_matches('<').trim_end_matches('>');
            match known.get(id) {
                Some(body) => {
                    let header = format!("222 0 <{id}> body\r\n");
                    if w.write_all(header.as_bytes()).await.is_err()
                        || write_dot_stuffed(&mut w, body).await.is_err()
                        || w.write_all(b".\r\n").await.is_err()
                    {
                        return;
                    }
                }
                None => {
                    if w.write_all(b"430 No such article\r\n").await.is_err() {
                        return;
                    }
                }
            }
        } else if cmd == "QUIT" {
            let _ = w.write_all(b"205 bye\r\n").await;
            return;
        } else if w.write_all(b"500 unknown command\r\n").await.is_err() {
            return;
        }
    }
}

async fn write_dot_stuffed(w: &mut OwnedWriteHalf, body: &[u8]) -> std::io::Result<()> {
    for line in body.split_inclusive(|&b| b == b'\n') {
        if line.starts_with(b".") {
            w.write_all(b".").await?;
        }
        w.write_all(line).await?;
    }
    Ok(())
}

fn server_entry(addr: SocketAddr) -> ServerEntry {
    ServerEntry {
        host: addr.ip().to_string(),
        port: addr.port(),
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        retry_delay: 0,
        timeout: 5,
        proxy: None,
    }
}

/// A poisoned session cache makes session derivation fail locally on tier 1;
/// the download must abort with that error and never ask tier 2.
#[tokio::test]
async fn local_derivation_failure_aborts_without_tier2() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x51u8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"Payload behind a locally failing derivation.";
    let mut body = Vec::new();
    let encoded = UploadEncryptionAdapter::new(session)
        .encode_article(
            "local.bin",
            payload.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128,
            1,
            &mut body,
        )
        .expect("encode article failed");

    let counters = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
    let tiers: Vec<ServerTier> = counters
        .iter()
        .map(|counter| {
            let mut known = HashMap::new();
            known.insert("local-msg-01".to_string(), encoded.body.clone());
            ServerTier::solo(server_entry(spawn_fake_server_counting(
                known,
                counter.clone(),
            )))
        })
        .collect();

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "local.bin".to_string(),
            segments: vec![QueuedSegment {
                message_id: "local-msg-01".to_string(),
                part: 1,
                bytes: payload.len() as u64,
            }],
        }],
    };
    let dest = tempfile::tempdir().unwrap();
    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(kdf_input));
    decryptor.poison_session_cache_for_tests();

    let err = download_queue_with_decryptor(&queue, &tiers, dest.path(), 0, None, Some(decryptor))
        .await
        .expect_err("local crypto failure must abort the download");

    assert!(
        err.to_string().contains("session cache lock poisoned"),
        "got: {err:#}"
    );
    assert_eq!(counters[0].load(Ordering::SeqCst), 1, "tier 1 tried once");
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "tier 2 must receive zero requests after a local failure"
    );
    assert!(!dest.path().join("local.bin").exists());
}

/// PENNE14-01: the same local failure hit through the cache prepass (a
/// cached valid encrypted article, poisoned session cache) must abort
/// immediately instead of falling through to a provider fetch.
#[tokio::test]
async fn local_crypto_failure_in_cache_prepass_aborts_without_any_fetch() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x52u8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"Cached payload behind a locally failing derivation.";
    let mut body = Vec::new();
    let encoded = UploadEncryptionAdapter::new(session)
        .encode_article(
            "cached-local.bin",
            payload.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128,
            1,
            &mut body,
        )
        .expect("encode article failed");

    let counters = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
    let tiers: Vec<ServerTier> = counters
        .iter()
        .map(|counter| {
            let mut known = HashMap::new();
            known.insert("cached-local-msg-01".to_string(), encoded.body.clone());
            ServerTier::solo(server_entry(spawn_fake_server_counting(
                known,
                counter.clone(),
            )))
        })
        .collect();

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "cached-local.bin".to_string(),
            segments: vec![QueuedSegment {
                message_id: "cached-local-msg-01".to_string(),
                part: 1,
                bytes: payload.len() as u64,
            }],
        }],
    };
    let dest = tempfile::tempdir().unwrap();
    cache::store(dest.path(), "cached-local-msg-01", &encoded.body).expect("seed article cache");
    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(kdf_input));
    decryptor.poison_session_cache_for_tests();

    let err = download_queue_with_decryptor(&queue, &tiers, dest.path(), 0, None, Some(decryptor))
        .await
        .expect_err("local crypto failure in cache prepass must abort the download");

    assert!(
        err.to_string().contains("session cache lock poisoned"),
        "got: {err:#}"
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "tier 1 must receive zero BODY requests after a local failure in the cache prepass"
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "tier 2 must receive zero BODY requests after a local failure in the cache prepass"
    );
    assert!(!dest.path().join("cached-local.bin").exists());
    assert!(!dest.path().join("cached-local.bin.penne-part").exists());
}
