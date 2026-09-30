//! Integration tests for Penne download decryption and layer encapsulation (ARCH-01, ARCH-03).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use pesto::config::ServerEntry;
use pesto::crypto::kdf::EncryptionSession;
use pesto::crypto::{DownloadDecryptionAdapter, UploadEncryptionAdapter};
use pesto::poster::SegmentIdentity;
use pesto::yenc::{encode_part, PartSpec};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};

use penne::config::ServerTier;
use penne::download::{download_queue, download_queue_with_decryptor};
use penne::queue::{DownloadQueue, QueuedFile, QueuedSegment};

/// Spawn a fake NNTP server that only understands `BODY` and `QUIT`.
fn spawn_fake_server(known: HashMap<String, Vec<u8>>) -> SocketAddr {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let listener = TcpListener::from_std(std_listener).unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(handle_connection(stream, known.clone()));
        }
    });

    addr
}

async fn handle_connection(stream: TcpStream, known: HashMap<String, Vec<u8>>) {
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

#[tokio::test]
async fn test_download_adapter_encapsulation() {
    let password = "penne-test-password-123";
    let salt = [0x77u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let uploader = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Plaintext content for Penne download encapsulation test. 1234567890.";
    let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };

    let mut encoded_body = Vec::new();
    let encoded = uploader
        .encode_article(
            "encapsulated.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            None,
            identity,
            &mut encoded_body,
        )
        .expect("upload adapter encode failed");

    // Serve via mock NNTP server
    let mut known = HashMap::new();
    known.insert("enc-msg-01".to_string(), encoded.body.clone());
    let addr = spawn_fake_server(known);

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "encapsulated.bin".to_string(),
            file_ordinal: Some(1),
            total_files: Some(1),
            segments: vec![QueuedSegment {
                message_id: "enc-msg-01".to_string(),
                part: 1,
                bytes: payload.len() as u64,
                segment_index: Some(1),
            }],
        }],
    };

    let dest_dir = tempfile::tempdir().unwrap();
    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(password));

    // Execute download with decryptor
    let outcome = download_queue_with_decryptor(
        &queue,
        &[ServerTier::solo(server_entry(addr))],
        dest_dir.path(),
        0,
        None,
        Some(decryptor),
    )
    .await
    .expect("download_queue_with_decryptor failed");

    assert!(outcome.segments.contains("enc-msg-01"));
    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());

    // Verify file content on disk is exact plaintext
    let output_file = dest_dir.path().join("encapsulated.bin");
    assert!(output_file.exists(), "assembled file must exist on disk");
    let disk_bytes = std::fs::read(&output_file).unwrap();
    assert_eq!(
        disk_bytes, payload,
        "disk file must match original plaintext"
    );

    // ARCH-03: Verify StreamingAssembly has no crypto imports or cipher references
    let assemble_src = include_str!("../src/assemble.rs");
    assert!(
        !assemble_src.contains("use pesto::crypto"),
        "assemble.rs must not import pesto::crypto"
    );
    assert!(
        !assemble_src.contains("XChaCha20"),
        "assemble.rs must not contain cipher references"
    );
    assert!(
        !assemble_src.contains("Argon2"),
        "assemble.rs must not contain KDF references"
    );
    assert!(
        !assemble_src.contains("Poly1305"),
        "assemble.rs must not contain MAC references"
    );
    assert!(
        !assemble_src.contains("FF1"),
        "assemble.rs must not contain FF1 references"
    );

    // Verify unencrypted download works without decryptor (backward compatibility)
    let plain_payload = b"Unencrypted plain file content.";
    let plain_body = encode_part(
        "plain.bin",
        plain_payload.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        plain_payload,
        128,
        None,
    )
    .body;

    let mut plain_known = HashMap::new();
    plain_known.insert("plain-msg-01".to_string(), plain_body);
    let plain_addr = spawn_fake_server(plain_known);

    let plain_queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "plain.bin".to_string(),
            file_ordinal: None,
            total_files: None,
            segments: vec![QueuedSegment {
                message_id: "plain-msg-01".to_string(),
                part: 1,
                bytes: plain_payload.len() as u64,
                segment_index: None,
            }],
        }],
    };

    let plain_dest = tempfile::tempdir().unwrap();
    let plain_outcome = download_queue(
        &plain_queue,
        &[ServerTier::solo(server_entry(plain_addr))],
        plain_dest.path(),
        0,
        None,
    )
    .await
    .expect("unencrypted download failed");

    assert!(plain_outcome.segments.contains("plain-msg-01"));
    let plain_disk_bytes = std::fs::read(plain_dest.path().join("plain.bin")).unwrap();
    assert_eq!(plain_disk_bytes, plain_payload);
}

#[tokio::test]
async fn test_zero_output_on_auth_failure() {
    let password = "zero-output-test-password";
    let salt = [0xAAu8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let uploader = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Sensitive payload that must never touch disk if authentication fails!";
    let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };

    let mut valid_body = Vec::new();
    let encoded = uploader
        .encode_article(
            "sensitive.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            None,
            identity,
            &mut valid_body,
        )
        .expect("upload adapter encode failed");

    // Create a tampered copy: flip a byte in the data line (line index 2, 0-based),
    // ensuring control lines (lines 0, 1, and the last line) remain valid FF1 ciphertexts.
    let mut lines: Vec<Vec<u8>> = encoded
        .body
        .split_inclusive(|&b| b == b'\n')
        .map(|l| l.to_vec())
        .collect();
    assert!(
        lines.len() >= 4,
        "must have header, encryption, data, and footer lines"
    );
    // Mutate a byte in the data line (lines[2])
    let data_len = lines[2].len();
    assert!(data_len > 4, "data line must have content");
    lines[2][0] ^= 0x01;
    let tampered_body: Vec<u8> = lines.into_iter().flatten().collect();

    // 1. Failover test: Server 1 (Tier 1) has tampered body, Server 2 (Tier 2) has valid body
    let mut s1_known = HashMap::new();
    s1_known.insert("failover-msg-01".to_string(), tampered_body.clone());
    let s1_addr = spawn_fake_server(s1_known);

    let mut s2_known = HashMap::new();
    s2_known.insert("failover-msg-01".to_string(), encoded.body.clone());
    let s2_addr = spawn_fake_server(s2_known);

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "sensitive.bin".to_string(),
            file_ordinal: Some(1),
            total_files: Some(1),
            segments: vec![QueuedSegment {
                message_id: "failover-msg-01".to_string(),
                part: 1,
                bytes: payload.len() as u64,
                segment_index: Some(1),
            }],
        }],
    };

    let dest_dir = tempfile::tempdir().unwrap();
    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(password));

    let tiers = vec![
        ServerTier::solo(server_entry(s1_addr)),
        ServerTier::solo(server_entry(s2_addr)),
    ];

    let outcome = download_queue_with_decryptor(
        &queue,
        &tiers,
        dest_dir.path(),
        0,
        None,
        Some(decryptor.clone()),
    )
    .await
    .expect("download_queue_with_decryptor failed");

    // S1 failed auth -> failed over to S2 -> S2 succeeded
    assert!(
        outcome.segments.contains("failover-msg-01"),
        "segment should be resolved by backup server"
    );
    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());

    let output_file = dest_dir.path().join("sensitive.bin");
    assert!(output_file.exists());
    let disk_bytes = std::fs::read(&output_file).unwrap();
    assert_eq!(
        disk_bytes, payload,
        "assembled file on disk must match plaintext"
    );

    // 2. Zero-output test: only Server 1 exists with tampered body
    let mut only_bad_known = HashMap::new();
    only_bad_known.insert("bad-msg-01".to_string(), tampered_body);
    let bad_addr = spawn_fake_server(only_bad_known);

    let bad_queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "must_not_exist.bin".to_string(),
            file_ordinal: Some(1),
            total_files: Some(1),
            segments: vec![QueuedSegment {
                message_id: "bad-msg-01".to_string(),
                part: 1,
                bytes: payload.len() as u64,
                segment_index: Some(1),
            }],
        }],
    };

    let bad_dest_dir = tempfile::tempdir().unwrap();
    let bad_outcome = download_queue_with_decryptor(
        &bad_queue,
        &[ServerTier::solo(server_entry(bad_addr))],
        bad_dest_dir.path(),
        0,
        None,
        Some(decryptor),
    )
    .await
    .expect("download should finish with corrupt segment reported");

    // Segment was corrupt
    assert!(
        bad_outcome.segments.is_empty(),
        "no segments should be recorded as successfully fetched"
    );
    assert_eq!(bad_outcome.corrupt.len(), 1);
    assert_eq!(bad_outcome.corrupt[0].message_id, "bad-msg-01");
    assert!(
        bad_outcome.corrupt[0]
            .error
            .contains("AUTHENTICATION_FAILURE")
            || bad_outcome.corrupt[0]
                .error
                .contains("ciphertext CRC mismatch"),
        "error must indicate authentication failure: {}",
        bad_outcome.corrupt[0].error
    );

    // ZERO-OUTPUT GUARANTEE: Neither final file nor temp file must exist on disk
    let final_path = bad_dest_dir.path().join("must_not_exist.bin");
    assert!(
        !final_path.exists(),
        "final file must not exist on authentication failure"
    );
    let tmp_path = bad_dest_dir.path().join("must_not_exist.bin.tmp");
    assert!(
        !tmp_path.exists(),
        "temp file must not exist on authentication failure"
    );

    // Verify cache was not written for failed authentication
    assert!(
        penne::cache::load(bad_dest_dir.path(), "bad-msg-01").is_none(),
        "cache must not store unauthenticated body"
    );
}

#[tokio::test]
async fn test_bad_cache_eviction_and_refetch() {
    let password = "cache-eviction-password";
    let salt = [0x55u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let uploader = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Payload for testing cache eviction and refetch on corruption!";
    let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };

    let mut valid_body = Vec::new();
    let encoded = uploader
        .encode_article(
            "cache_test.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            None,
            identity,
            &mut valid_body,
        )
        .expect("encode failed");

    let dest_dir = tempfile::tempdir().unwrap();

    // Seed cache with corrupted garbage
    let corrupted_cache_bytes = b"CORRUPTED_CACHE_ENTRY_NOT_VALID_YENC";
    penne::cache::store(dest_dir.path(), "cache-evict-msg-01", corrupted_cache_bytes).unwrap();
    assert!(penne::cache::load(dest_dir.path(), "cache-evict-msg-01").is_some());

    // Mock server serves valid body
    let mut known = HashMap::new();
    known.insert("cache-evict-msg-01".to_string(), encoded.body.clone());
    let addr = spawn_fake_server(known);

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "cache_test.bin".to_string(),
            file_ordinal: Some(1),
            total_files: Some(1),
            segments: vec![QueuedSegment {
                message_id: "cache-evict-msg-01".to_string(),
                part: 1,
                bytes: payload.len() as u64,
                segment_index: Some(1),
            }],
        }],
    };

    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(password));
    let outcome = download_queue_with_decryptor(
        &queue,
        &[ServerTier::solo(server_entry(addr))],
        dest_dir.path(),
        0,
        None,
        Some(decryptor),
    )
    .await
    .expect("download_queue_with_decryptor failed");

    assert!(outcome.segments.contains("cache-evict-msg-01"));
    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());

    // File assembled matches plaintext
    let output_file = dest_dir.path().join("cache_test.bin");
    assert!(output_file.exists());
    assert_eq!(std::fs::read(&output_file).unwrap(), payload);

    // Cache now holds valid body, not the corrupted one
    let cached_after = penne::cache::load(dest_dir.path(), "cache-evict-msg-01").unwrap();
    assert_eq!(cached_after, encoded.body);
}

#[tokio::test]
async fn test_tampered_primary_failover_to_secondary() {
    let password = "tier-failover-password";
    let salt = [0x66u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let uploader = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Payload for multi-tier failover verification under provider tampering!";
    let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };

    let mut valid_body = Vec::new();
    let encoded = uploader
        .encode_article(
            "failover_test.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            None,
            identity,
            &mut valid_body,
        )
        .expect("encode failed");

    // Tamper the ciphertext data line
    let mut lines: Vec<Vec<u8>> = encoded
        .body
        .split_inclusive(|&b| b == b'\n')
        .map(|l| l.to_vec())
        .collect();
    lines[2][0] ^= 0x01;
    let tampered_body: Vec<u8> = lines.into_iter().flatten().collect();

    // S1 (Tier 1) has tampered body, S2 (Tier 2) has valid body
    let mut s1_known = HashMap::new();
    s1_known.insert("tier-failover-01".to_string(), tampered_body);
    let s1_addr = spawn_fake_server(s1_known);

    let mut s2_known = HashMap::new();
    s2_known.insert("tier-failover-01".to_string(), encoded.body.clone());
    let s2_addr = spawn_fake_server(s2_known);

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "failover_test.bin".to_string(),
            file_ordinal: Some(1),
            total_files: Some(1),
            segments: vec![QueuedSegment {
                message_id: "tier-failover-01".to_string(),
                part: 1,
                bytes: payload.len() as u64,
                segment_index: Some(1),
            }],
        }],
    };

    let dest_dir = tempfile::tempdir().unwrap();
    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(password));

    let tiers = vec![
        ServerTier::solo(server_entry(s1_addr)),
        ServerTier::solo(server_entry(s2_addr)),
    ];

    let outcome =
        download_queue_with_decryptor(&queue, &tiers, dest_dir.path(), 0, None, Some(decryptor))
            .await
            .expect("download should succeed on failover");

    assert!(outcome.segments.contains("tier-failover-01"));
    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());

    let output_file = dest_dir.path().join("failover_test.bin");
    assert!(output_file.exists());
    assert_eq!(std::fs::read(&output_file).unwrap(), payload);

    // Cache holds valid body from tier 2
    let cached = penne::cache::load(dest_dir.path(), "tier-failover-01").unwrap();
    assert_eq!(cached, encoded.body);
}

#[tokio::test]
async fn test_multipart_partial_auth_failure_cleanup() {
    let password = "multipart-partial-auth-fail-password";
    let salt = [0x77u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let uploader = UploadEncryptionAdapter::new(session.clone());

    let part1_payload = vec![0x11u8; 500];
    let part2_payload = vec![0x22u8; 500];

    let id1 = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
    let id2 = SegmentIdentity::checked(0, 1, 1, 2).unwrap();

    let mut body1 = Vec::new();
    let enc1 = uploader
        .encode_article(
            "multipart_cleanup.bin",
            1000,
            PartSpec {
                number: 1,
                total: 2,
                offset: 0,
            },
            &part1_payload,
            128,
            None,
            id1,
            &mut body1,
        )
        .unwrap();

    let mut body2 = Vec::new();
    let enc2 = uploader
        .encode_article(
            "multipart_cleanup.bin",
            1000,
            PartSpec {
                number: 2,
                total: 2,
                offset: 500,
            },
            &part2_payload,
            128,
            None,
            id2,
            &mut body2,
        )
        .unwrap();

    // Tamper part 2
    let mut lines2: Vec<Vec<u8>> = enc2
        .body
        .split_inclusive(|&b| b == b'\n')
        .map(|l| l.to_vec())
        .collect();
    lines2[3][0] ^= 0x01; // multipart has =ybegin (0), =ypart (1), =yencryption (2), data (3)
    let tampered_body2: Vec<u8> = lines2.into_iter().flatten().collect();

    let mut known = HashMap::new();
    known.insert("mp-msg-01".to_string(), enc1.body);
    known.insert("mp-msg-02".to_string(), tampered_body2);
    let addr = spawn_fake_server(known);

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "multipart_cleanup.bin".to_string(),
            file_ordinal: Some(1),
            total_files: Some(1),
            segments: vec![
                QueuedSegment {
                    message_id: "mp-msg-01".to_string(),
                    part: 1,
                    bytes: 500,
                    segment_index: Some(1),
                },
                QueuedSegment {
                    message_id: "mp-msg-02".to_string(),
                    part: 2,
                    bytes: 500,
                    segment_index: Some(2),
                },
            ],
        }],
    };

    let dest_dir = tempfile::tempdir().unwrap();
    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(password));

    let outcome = download_queue_with_decryptor(
        &queue,
        &[ServerTier::solo(server_entry(addr))],
        dest_dir.path(),
        0,
        None,
        Some(decryptor),
    )
    .await
    .expect("download finishes");

    // Part 1 succeeded, Part 2 was corrupt
    assert!(outcome.segments.contains("mp-msg-01"));
    assert_eq!(outcome.corrupt.len(), 1);
    assert_eq!(outcome.corrupt[0].message_id, "mp-msg-02");

    // ZERO-OUTPUT GUARANTEE:
    // Neither final file nor temporary file must exist on disk!
    let final_path = dest_dir.path().join("multipart_cleanup.bin");
    assert!(!final_path.exists(), "final file must not exist");

    let tmp_path = dest_dir.path().join("multipart_cleanup.bin.tmp");
    assert!(
        !tmp_path.exists(),
        "temporary partial file must be cleaned up"
    );
}
