//! PENNE14-02: an intact part-1 body replayed for part 2's Message-ID
//! authenticates, but it is not part 2. The encrypted download path must
//! reject it as ordinary provider corruption (failover/corrupt), never
//! marking both parts present and producing a zero-filled second half
//! reported as `Complete`/`CompleteUnverified`, and never committing the
//! duplicate as part 2's cache resolution. (Separate file because
//! `download_encryption.rs` is pinned at its source-size baseline.)
//!
//! Harness mirrors `download_local_crypto_failure.rs`'s self-contained
//! fake NNTP server.

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
use penne::assemble::AssembleOutcome;
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

/// Two equal-length encrypted parts; the part-1 body is served for both
/// Message-IDs on both tiers. AEAD authenticates the duplicate, so only
/// the receiver's part-coverage check can stop it (Body §2.1/§7).
#[tokio::test]
async fn replayed_first_part_for_both_message_ids_fails_part_two() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x77u8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let part1 = vec![0x41u8; 64];
    let mut body = Vec::new();
    let art1 = UploadEncryptionAdapter::new(session)
        .encode_article(
            "replay.bin",
            128,
            PartSpec {
                number: 1,
                total: 2,
                offset: 0,
            },
            &part1,
            128,
            1,
            &mut body,
        )
        .expect("encode article failed")
        .body;

    let counters = [Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0))];
    let tiers: Vec<ServerTier> = counters
        .iter()
        .map(|c| {
            let mut known = HashMap::new();
            known.insert("replay-msg-01".into(), art1.clone());
            known.insert("replay-msg-02".into(), art1.clone());
            ServerTier::solo(server_entry(spawn_fake_server_counting(known, c.clone())))
        })
        .collect();
    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "replay.bin".to_string(),
            segments: vec![
                QueuedSegment {
                    message_id: "replay-msg-01".into(),
                    part: 1,
                    bytes: 64,
                },
                QueuedSegment {
                    message_id: "replay-msg-02".into(),
                    part: 2,
                    bytes: 64,
                },
            ],
        }],
    };
    let dest = tempfile::tempdir().unwrap();
    let outcome = download_queue_with_decryptor(
        &queue,
        &tiers,
        dest.path(),
        0,
        None,
        Some(Arc::new(DownloadDecryptionAdapter::with_password(
            kdf_input,
        ))),
    )
    .await
    .unwrap();

    // Part 1 resolves on tier 1; only the mismatching part 2 fails over.
    assert_eq!(counters[0].load(Ordering::SeqCst), 2);
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "only part 2 fails over"
    );
    assert!(outcome.missing.is_empty());
    assert_eq!(outcome.corrupt.len(), 1);
    assert_eq!(outcome.corrupt[0].message_id, "replay-msg-02");
    assert!(
        outcome.corrupt[0].error.contains("PART_MISMATCH"),
        "got: {}",
        outcome.corrupt[0].error
    );
    assert!(outcome.segments.contains("replay-msg-01"));
    assert!(!outcome.segments.contains("replay-msg-02"));

    // No Complete/CompleteUnverified (zero-filled second half) output:
    // assembly reports part 2 missing and discards the temp file.
    assert_eq!(
        outcome.assembled.get("replay.bin"),
        Some(&AssembleOutcome::Incomplete {
            missing_parts: vec![2]
        })
    );
    assert!(!dest.path().join("replay.bin").exists());
    assert!(!dest.path().join("replay.bin.penne-part").exists());

    // The duplicate is not committed as part 2's cache resolution.
    assert!(penne::cache::load(dest.path(), "replay-msg-01").is_some());
    assert!(
        penne::cache::load(dest.path(), "replay-msg-02").is_none(),
        "duplicate part 1 must not be committed as part 2's cache copy"
    );
}
