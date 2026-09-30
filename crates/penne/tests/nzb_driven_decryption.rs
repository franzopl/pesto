//! Integration tests for Penne NZB-driven decryption (Phase 14).
//!
//! Validates mock-NNTP download, provider failover, zero-output tampering checks,
//! cache stability, and preflight validation using only explicit segment indices
//! and article bodies.

mod support;

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use penne::config::ServerTier;
use penne::download::{download_queue_with_decryptor, DownloadOutcome};
use penne::nzb::{download_decryptor, load};
use penne::queue::{build, DownloadQueue, QueuedFile, QueuedSegment};
use pesto::crypto::kdf::EncryptionSession;
use pesto::crypto::{DownloadDecryptionAdapter, UploadEncryptionAdapter};
use pesto::poster::SegmentIdentity;
use pesto::yenc::PartSpec;
use tempfile::NamedTempFile;

use support::{server_entry, spawn_mock_nntp_server};

fn write_temp_nzb(xml: &str) -> NamedTempFile {
    let mut temp = NamedTempFile::new().expect("failed to create temp file");
    temp.write_all(xml.as_bytes()).expect("failed to write XML");
    temp
}

fn encrypted_nzb_xml(password: &str, file_subject: &str, segments: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{password}</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="uploader@example.com" date="1774300000" subject="{file_subject}">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      {segments}
    </segments>
  </file>
</nzb>"#
    )
}

async fn run_download_nzb(xml: &str, tiers: &[ServerTier], dest: &Path) -> DownloadOutcome {
    let nzb_file = write_temp_nzb(xml);
    let parsed = load(nzb_file.path()).unwrap();
    let queue = build(&parsed);
    let decryptor = download_decryptor(&parsed.meta).unwrap();
    download_queue_with_decryptor(&queue, tiers, dest, 0, None, decryptor)
        .await
        .unwrap()
}

fn encode_test_article(
    session: &Arc<EncryptionSession>,
    file_name: &str,
    file_size: u64,
    spec: PartSpec,
    payload: &[u8],
    segment_index: u32,
) -> Vec<u8> {
    let uploader = UploadEncryptionAdapter::new(session.clone());
    let identity = SegmentIdentity::explicit(0, 0, spec.number, segment_index).unwrap();
    let mut body = Vec::new();
    let encoded = uploader
        .encode_article(
            file_name, file_size, spec, payload, 128, None, identity, &mut body,
        )
        .expect("encode article failed");
    encoded.body
}

#[tokio::test]
async fn test_download_obfuscated_and_arbitrary_subjects() {
    let password = "test-pass-obfuscated-123";
    let salt = [0x31u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());

    let payload_p1 = b"First half of payload with arbitrary obfuscated subject string. ";
    let payload_p2 = b"Second half of payload with arbitrary obfuscated subject string.";
    let mut original = Vec::new();
    original.extend_from_slice(payload_p1);
    original.extend_from_slice(payload_p2);
    let total_len = original.len() as u64;

    let art1 = encode_test_article(
        &session,
        "obf.bin",
        total_len,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        payload_p1,
        101,
    );
    let art2 = encode_test_article(
        &session,
        "obf.bin",
        total_len,
        PartSpec {
            number: 2,
            total: 2,
            offset: payload_p1.len() as u64,
        },
        payload_p2,
        102,
    );

    let mut known = HashMap::new();
    known.insert("msg-obf-01@test".to_string(), art1);
    known.insert("msg-obf-02@test".to_string(), art2);
    let addr = spawn_mock_nntp_server(known, None);

    let segments = format!(
        "<segment bytes=\"{}\" number=\"1\" segmentIndex=\"101\">msg-obf-01@test</segment>\n\
         <segment bytes=\"{}\" number=\"2\" segmentIndex=\"102\">msg-obf-02@test</segment>",
        payload_p1.len(),
        payload_p2.len()
    );
    let xml = encrypted_nzb_xml(
        password,
        "completely-obfuscated-random-subject-no-counters",
        &segments,
    );
    let dest = tempfile::tempdir().unwrap();
    let outcome =
        run_download_nzb(&xml, &[ServerTier::solo(server_entry(addr))], dest.path()).await;

    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());
    assert_eq!(outcome.segments.len(), 2);

    let file_path = dest
        .path()
        .join("completely-obfuscated-random-subject-no-counters");
    assert!(file_path.exists());
    assert_eq!(std::fs::read(&file_path).unwrap(), original);
}

#[tokio::test]
async fn test_download_misleading_subject_counters() {
    let password = "test-pass-misleading-456";
    let salt = [0x32u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());

    let payload_p1 = b"Chunk 1 with misleading subject counters in NZB header.";
    let payload_p2 = b"Chunk 2 with misleading subject counters in NZB header.";
    let mut original = Vec::new();
    original.extend_from_slice(payload_p1);
    original.extend_from_slice(payload_p2);
    let total_len = original.len() as u64;

    let art1 = encode_test_article(
        &session,
        "misleading.bin",
        total_len,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        payload_p1,
        1,
    );
    let art2 = encode_test_article(
        &session,
        "misleading.bin",
        total_len,
        PartSpec {
            number: 2,
            total: 2,
            offset: payload_p1.len() as u64,
        },
        payload_p2,
        2,
    );

    let mut known = HashMap::new();
    known.insert("msg-mis-01@test".to_string(), art1);
    known.insert("msg-mis-02@test".to_string(), art2);
    let addr = spawn_mock_nntp_server(known, None);

    let segments = format!(
        "<segment bytes=\"{}\" number=\"1\" segmentIndex=\"1\">msg-mis-01@test</segment>\n\
         <segment bytes=\"{}\" number=\"2\" segmentIndex=\"2\">msg-mis-02@test</segment>",
        payload_p1.len(),
        payload_p2.len()
    );
    let xml = encrypted_nzb_xml(
        password,
        "[99/100] - &quot;misleading.bin&quot; yEnc",
        &segments,
    );
    let dest = tempfile::tempdir().unwrap();
    let outcome =
        run_download_nzb(&xml, &[ServerTier::solo(server_entry(addr))], dest.path()).await;

    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());

    let file_path = dest.path().join("misleading.bin");
    assert!(file_path.exists());
    assert_eq!(std::fs::read(&file_path).unwrap(), original);
}

#[tokio::test]
async fn test_download_reordered_files_and_segments() {
    let password = "test-pass-reordered-789";
    let salt = [0x33u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());

    let payload_f1 = b"Payload for file A (part 1 only)";
    let payload_f2_p1 = b"Payload for file B part 1 of 2.";
    let payload_f2_p2 = b"Payload for file B part 2 of 2.";
    let mut original_f2 = Vec::new();
    original_f2.extend_from_slice(payload_f2_p1);
    original_f2.extend_from_slice(payload_f2_p2);

    let art_f1 = encode_test_article(
        &session,
        "file_a.bin",
        payload_f1.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload_f1,
        10,
    );
    let art_f2_p1 = encode_test_article(
        &session,
        "file_b.bin",
        original_f2.len() as u64,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        payload_f2_p1,
        20,
    );
    let art_f2_p2 = encode_test_article(
        &session,
        "file_b.bin",
        original_f2.len() as u64,
        PartSpec {
            number: 2,
            total: 2,
            offset: payload_f2_p1.len() as u64,
        },
        payload_f2_p2,
        21,
    );

    let mut known = HashMap::new();
    known.insert("msg-f1@test".to_string(), art_f1);
    known.insert("msg-f2-p1@test".to_string(), art_f2_p1);
    known.insert("msg-f2-p2@test".to_string(), art_f2_p2);
    let addr = spawn_mock_nntp_server(known, None);

    // List file B before file A, and inside file B list segment 2 before segment 1
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{password}</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;file_b.bin&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="2" segmentIndex="21">msg-f2-p2@test</segment>
      <segment bytes="{}" number="1" segmentIndex="20">msg-f2-p1@test</segment>
    </segments>
  </file>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;file_a.bin&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="1" segmentIndex="10">msg-f1@test</segment>
    </segments>
  </file>
</nzb>"#,
        payload_f2_p2.len(),
        payload_f2_p1.len(),
        payload_f1.len()
    );

    let dest = tempfile::tempdir().unwrap();
    let outcome =
        run_download_nzb(&xml, &[ServerTier::solo(server_entry(addr))], dest.path()).await;

    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());
    assert_eq!(
        std::fs::read(dest.path().join("file_a.bin")).unwrap(),
        payload_f1
    );
    assert_eq!(
        std::fs::read(dest.path().join("file_b.bin")).unwrap(),
        original_f2
    );
}

#[tokio::test]
async fn test_download_sparse_subsets() {
    let password = "test-pass-sparse-subsets";
    let salt = [0x34u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());

    let payload1 = b"Sparse payload item 1 with index 3";
    let payload2 = b"Sparse payload item 2 with index 7";

    let art1 = encode_test_article(
        &session,
        "sparse1.bin",
        payload1.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload1,
        3,
    );
    let art2 = encode_test_article(
        &session,
        "sparse2.bin",
        payload2.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload2,
        7,
    );

    let mut known = HashMap::new();
    known.insert("msg-sparse-03@test".to_string(), art1);
    known.insert("msg-sparse-07@test".to_string(), art2);
    let addr = spawn_mock_nntp_server(known, None);

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{password}</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;sparse1.bin&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="1" segmentIndex="3">msg-sparse-03@test</segment>
    </segments>
  </file>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;sparse2.bin&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="1" segmentIndex="7">msg-sparse-07@test</segment>
    </segments>
  </file>
</nzb>"#,
        payload1.len(),
        payload2.len()
    );

    let dest = tempfile::tempdir().unwrap();
    let outcome =
        run_download_nzb(&xml, &[ServerTier::solo(server_entry(addr))], dest.path()).await;

    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());
    assert_eq!(
        std::fs::read(dest.path().join("sparse1.bin")).unwrap(),
        payload1
    );
    assert_eq!(
        std::fs::read(dest.path().join("sparse2.bin")).unwrap(),
        payload2
    );
}

#[tokio::test]
async fn test_download_provider_failover() {
    let password = "test-pass-failover-tier";
    let salt = [0x35u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());

    let payload = b"Plaintext for provider failover recovery verification.";
    let valid_art = encode_test_article(
        &session,
        "failover.bin",
        payload.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload,
        50,
    );

    // Create a corrupted version for Tier 1: mutate a byte in the body
    let mut corrupt_art = valid_art.clone();
    let body_pos = corrupt_art
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|p| p + 2)
        .unwrap_or(0);
    if body_pos + 10 < corrupt_art.len() {
        corrupt_art[body_pos + 10] ^= 0x55;
    }

    let mut known_tier1 = HashMap::new();
    known_tier1.insert("msg-failover@test".to_string(), corrupt_art);
    let addr_tier1 = spawn_mock_nntp_server(known_tier1, None);

    let mut known_tier2 = HashMap::new();
    known_tier2.insert("msg-failover@test".to_string(), valid_art.clone());
    let addr_tier2 = spawn_mock_nntp_server(known_tier2, None);

    let segments = format!(
        "<segment bytes=\"{}\" number=\"1\" segmentIndex=\"50\">msg-failover@test</segment>",
        payload.len()
    );
    let xml = encrypted_nzb_xml(password, "&quot;failover.bin&quot; yEnc", &segments);
    let dest = tempfile::tempdir().unwrap();

    let tiers = vec![
        ServerTier::solo(server_entry(addr_tier1)),
        ServerTier::solo(server_entry(addr_tier2)),
    ];
    let outcome = run_download_nzb(&xml, &tiers, dest.path()).await;

    assert!(outcome.corrupt.is_empty(), "failover must resolve cleanly");
    assert!(outcome
        .segments
        .iter()
        .any(|s| s.contains("msg-failover@test")));
    assert_eq!(
        std::fs::read(dest.path().join("failover.bin")).unwrap(),
        payload
    );

    // Verify cache holds genuine Tier 2 bytes, not corrupted Tier 1 bytes
    let cached = penne::cache::load(dest.path(), "msg-failover@test").unwrap();
    assert_eq!(cached, valid_art);
}

#[tokio::test]
async fn test_download_index_tampering_zero_output() {
    let password = "test-pass-tampering-zero-out";
    let salt = [0x36u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());

    let payload = b"Sensitive content that must NEVER appear on disk if tampered.";
    // Wire article encoded with authentic segment_index = 1
    let genuine_art = encode_test_article(
        &session,
        "tamper.bin",
        payload.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload,
        1,
    );

    let mut known = HashMap::new();
    known.insert("msg-tamper@test".to_string(), genuine_art);
    let addr = spawn_mock_nntp_server(known, None);

    // NZB specifies segmentIndex = 2 (tampered index)
    let segments = format!(
        "<segment bytes=\"{}\" number=\"1\" segmentIndex=\"2\">msg-tamper@test</segment>",
        payload.len()
    );
    let xml = encrypted_nzb_xml(password, "&quot;tamper.bin&quot; yEnc", &segments);
    let dest = tempfile::tempdir().unwrap();

    let outcome =
        run_download_nzb(&xml, &[ServerTier::solo(server_entry(addr))], dest.path()).await;

    // Must report corruption due to AEAD authentication failure
    assert_eq!(outcome.corrupt.len(), 1);
    assert!(outcome.segments.is_empty());

    // Zero-Output Guarantee:
    // 1. Destination final file does NOT exist
    assert!(
        !dest.path().join("tamper.bin").exists(),
        "final file must not exist on authentication failure"
    );
    // 2. Destination temporary file does NOT exist
    assert!(
        !dest.path().join("tamper.bin.tmp").exists(),
        "temporary file must not exist on authentication failure"
    );
    // 3. Cache entry does NOT exist
    assert!(
        penne::cache::load(dest.path(), "msg-tamper@test").is_none(),
        "cache entry must not exist on authentication failure"
    );
}

#[tokio::test]
async fn test_cache_stability_and_reparse_restart() {
    let password = "test-pass-cache-stability";
    let salt = [0x37u8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());

    let payload_p1 = b"Part 1 for cache stability and restart verification.";
    let payload_p2 = b"Part 2 for cache stability and restart verification.";
    let mut original = Vec::new();
    original.extend_from_slice(payload_p1);
    original.extend_from_slice(payload_p2);
    let total_len = original.len() as u64;

    let art1 = encode_test_article(
        &session,
        "cache_stable.bin",
        total_len,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        payload_p1,
        5,
    );
    let art2 = encode_test_article(
        &session,
        "cache_stable.bin",
        total_len,
        PartSpec {
            number: 2,
            total: 2,
            offset: payload_p1.len() as u64,
        },
        payload_p2,
        6,
    );

    let mut known = HashMap::new();
    known.insert("msg-cache-01@test".to_string(), art1);
    known.insert("msg-cache-02@test".to_string(), art2);
    let req_counter = Arc::new(AtomicUsize::new(0));
    let addr = spawn_mock_nntp_server(known, Some(req_counter.clone()));

    let segments_orig = format!(
        "<segment bytes=\"{}\" number=\"1\" segmentIndex=\"5\">msg-cache-01@test</segment>\n\
         <segment bytes=\"{}\" number=\"2\" segmentIndex=\"6\">msg-cache-02@test</segment>",
        payload_p1.len(),
        payload_p2.len()
    );
    let xml_original = encrypted_nzb_xml(
        password,
        "&quot;cache_stable.bin&quot; yEnc",
        &segments_orig,
    );
    let dest = tempfile::tempdir().unwrap();

    // Run 1: Download from mock NNTP and populate cache
    let outcome1 = run_download_nzb(
        &xml_original,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
    )
    .await;

    assert!(outcome1.missing.is_empty());
    assert_eq!(outcome1.segments.len(), 2);
    assert_eq!(req_counter.load(Ordering::SeqCst), 2);

    // Verify cache entries exist
    assert!(penne::cache::load(dest.path(), "msg-cache-01@test").is_some());
    assert!(penne::cache::load(dest.path(), "msg-cache-02@test").is_some());

    // Prepare Run 2: Remove assembled file, but preserve .penne-cache
    std::fs::remove_file(dest.path().join("cache_stable.bin")).unwrap();
    assert!(!dest.path().join("cache_stable.bin").exists());

    // Run 2: Reparse a reordered NZB of the same release
    let segments_reorder = format!(
        "<segment bytes=\"{}\" number=\"2\" segmentIndex=\"6\">msg-cache-02@test</segment>\n\
         <segment bytes=\"{}\" number=\"1\" segmentIndex=\"5\">msg-cache-01@test</segment>",
        payload_p2.len(),
        payload_p1.len()
    );
    let xml_reordered = encrypted_nzb_xml(
        password,
        "&quot;cache_stable.bin&quot; yEnc",
        &segments_reorder,
    );

    // Reset request counter before Run 2
    req_counter.store(0, Ordering::SeqCst);

    let outcome2 = run_download_nzb(
        &xml_reordered,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
    )
    .await;

    assert!(outcome2.missing.is_empty());
    assert_eq!(outcome2.segments.len(), 2);

    // Crucial check: 0 NNTP requests made — everything served directly from cache
    assert_eq!(
        req_counter.load(Ordering::SeqCst),
        0,
        "second run must make zero NNTP requests, serving 100% from cache"
    );

    // Reassembled file matches original plaintext byte-for-byte
    let written = std::fs::read(dest.path().join("cache_stable.bin")).unwrap();
    assert_eq!(written, original);
}

#[tokio::test]
async fn test_preflight_rejects_malformed_queue_before_effects() {
    let password = "test-pass-preflight";
    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(password));

    // Construct queue with duplicate segmentIndex = 1 across two distinct Message-IDs
    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "malformed.bin".to_string(),
            file_ordinal: Some(1),
            total_files: Some(1),
            segments: vec![
                QueuedSegment {
                    message_id: "pre-msg-01@test".to_string(),
                    part: 1,
                    bytes: 100,
                    segment_index: Some(1),
                },
                QueuedSegment {
                    message_id: "pre-msg-02@test".to_string(),
                    part: 2,
                    bytes: 100,
                    segment_index: Some(1),
                },
            ],
        }],
    };

    let req_counter = Arc::new(AtomicUsize::new(0));
    let addr = spawn_mock_nntp_server(HashMap::new(), Some(req_counter.clone()));
    let dest = tempfile::tempdir().unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let result = download_queue_with_decryptor(
        &queue,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
        0,
        Some(tx),
        Some(decryptor),
    )
    .await;

    // Must fail closed with preflight DUPLICATE_SEGMENT_INDEX
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("DUPLICATE_SEGMENT_INDEX"),
        "error must be DUPLICATE_SEGMENT_INDEX, got: {err_msg}"
    );

    // Assert zero progress events emitted before error
    assert!(
        rx.try_recv().is_err(),
        "no progress events must be emitted prior to preflight failure"
    );

    // Assert zero temporary or final files created
    let entry_count = std::fs::read_dir(dest.path()).unwrap().count();
    assert_eq!(
        entry_count, 0,
        "destination directory must have zero entries"
    );

    // Assert zero NNTP requests made
    assert_eq!(
        req_counter.load(Ordering::SeqCst),
        0,
        "zero NNTP requests must be made on preflight failure"
    );
}

#[tokio::test]
async fn test_unencrypted_article_rejection_for_encrypted_segment() {
    let password = "unencrypted-rejection-password";
    let plain_body = pesto::yenc::encode_part(
        "spoofed.bin",
        10,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        b"0123456789",
        128,
        None,
    )
    .body;

    let mut known = HashMap::new();
    known.insert("spoofed-msg-01".to_string(), plain_body);
    let addr = spawn_mock_nntp_server(known, None);

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "spoofed.bin".to_string(),
            file_ordinal: Some(1),
            total_files: Some(1),
            segments: vec![QueuedSegment {
                message_id: "spoofed-msg-01".to_string(),
                part: 1,
                bytes: 10,
                segment_index: Some(1),
            }],
        }],
    };

    let dest = tempfile::tempdir().unwrap();
    let decryptor = Arc::new(DownloadDecryptionAdapter::with_password(password));

    let outcome = download_queue_with_decryptor(
        &queue,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
        0,
        None,
        Some(decryptor),
    )
    .await
    .expect("download should complete with corrupt segment");

    assert!(outcome.segments.is_empty());
    assert_eq!(outcome.corrupt.len(), 1);
    assert!(outcome.corrupt[0].error.contains("UNAUTHENTICATED_ARTICLE"));
    assert!(!dest.path().join("spoofed.bin").exists());
}
