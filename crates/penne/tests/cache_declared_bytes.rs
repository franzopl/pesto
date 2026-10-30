//! PE-NEXT-5 regression: the encrypted declared-bytes predicate must apply
//! to cache hits exactly like fresh fetches (Body Std v1.2 §2.1).
//!
//! The cache prepass in `download.rs` resolves a cached article body without
//! the optional nonzero `bytes` comparison the fresh-fetch worker path
//! applies. These tests prove warm/cold divergence before the fix and the
//! shared predicate after it; the zero/unknown-bytes skip and ordinary
//! (unencrypted) cache behavior are covered in `download_encryption.rs` and
//! `download_with_failover.rs` respectively.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pesto::config::ServerEntry;
use pesto::crypto::{EncryptionSession, UploadEncryptionAdapter};
use pesto::yenc::{encode_part, PartSpec};

mod support;
use penne::config::ServerTier;
use penne::download::download_queue_with_decryptor;
use penne::nzb::{download_decryptor, load};
use support::fixture_kdf_input;

/// Fake NNTP server counting BODY requests; identical shape to the one in
/// `download_encryption.rs`, kept local so this scenario file stands alone.
fn spawn_fake_server_counting(
    known: HashMap<String, Vec<u8>>,
    counter: Arc<AtomicUsize>,
) -> std::net::SocketAddr {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let known = known.clone();
            let counter = Arc::clone(&counter);
            std::thread::spawn(move || {
                let Ok(mut writer) = stream.try_clone() else {
                    return;
                };
                let mut reader = std::io::BufReader::new(stream);
                if writer.write_all(b"200 mock ready\r\n").is_err() {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    use std::io::BufRead;
                    match reader.read_line(&mut line) {
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
                                if writer.write_all(header.as_bytes()).is_err()
                                    || write_dot_stuffed(&mut writer, body).is_err()
                                    || writer.write_all(b".\r\n").is_err()
                                {
                                    return;
                                }
                            }
                            None => {
                                if writer.write_all(b"430 No such article\r\n").is_err() {
                                    return;
                                }
                            }
                        }
                    } else if cmd == "QUIT" {
                        let _ = writer.write_all(b"205 bye\r\n");
                        return;
                    } else if writer.write_all(b"500 unknown command\r\n").is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

fn write_dot_stuffed(w: &mut impl std::io::Write, body: &[u8]) -> std::io::Result<()> {
    for line in body.split_inclusive(|&b| b == b'\n') {
        if line.starts_with(b".") {
            w.write_all(b".")?;
        }
        w.write_all(line)?;
    }
    Ok(())
}

fn server_entry(addr: std::net::SocketAddr) -> ServerEntry {
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

fn encrypted_nzb_xml(password: &str, subject: &str, segment: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{password}</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="uploader@example.com" date="1774300000" subject="{subject}">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      {segment}
    </segments>
  </file>
</nzb>"#
    )
}

fn encode_article(
    session: &Arc<EncryptionSession>,
    file_name: &str,
    file_size: u64,
    payload: &[u8],
    segment_index: u32,
) -> Vec<u8> {
    let uploader = UploadEncryptionAdapter::new(session.clone());
    let mut body = Vec::new();
    let encoded = uploader
        .encode_article(
            file_name,
            file_size,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128,
            segment_index,
            &mut body,
        )
        .expect("encode article failed");
    encoded.body
}

async fn run_download_nzb(
    xml: &str,
    tiers: &[ServerTier],
    dest: &std::path::Path,
) -> penne::download::DownloadOutcome {
    let parsed = load(write_temp_nzb(xml).path()).unwrap();
    let queue = penne::queue::build(&parsed);
    let decryptor = download_decryptor(&parsed.meta).unwrap();
    download_queue_with_decryptor(&queue, tiers, dest, 0, None, decryptor)
        .await
        .unwrap()
}

fn write_temp_nzb(xml: &str) -> tempfile::NamedTempFile {
    use std::io::Write;
    let mut temp = tempfile::NamedTempFile::new().expect("failed to create temp file");
    temp.write_all(xml.as_bytes()).expect("failed to write XML");
    temp
}

/// PE-NEXT-5 (red proof shape on the unfixed source): with a valid warm
/// cache, changing only the nonzero NZB declared bytes lets the cached hit
/// commit — diverging from the cold path, which rejects the same mismatch
/// with SEGMENT_BYTES_MISMATCH failover.
#[tokio::test]
async fn changed_declared_bytes_on_warm_cache_match_cold_failover() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x69u8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"Warm cache declared-bytes proof payload.";
    let encoded = encode_article(&session, "warm.bin", payload.len() as u64, payload, 1);

    let counter = Arc::new(AtomicUsize::new(0));
    let mut known = HashMap::new();
    known.insert("warm-msg-01".to_string(), encoded);
    let addr = spawn_fake_server_counting(known, counter.clone());

    let dest = tempfile::tempdir().unwrap();

    // Run 1 (cold, correct declared bytes): fetch and populate the cache.
    let xml_correct = encrypted_nzb_xml(
        kdf_input,
        "&quot;warm.bin&quot; yEnc",
        &format!(
            "<segment bytes=\"{}\" number=\"1\">warm-msg-01</segment>",
            payload.len()
        ),
    );
    let outcome1 = run_download_nzb(
        &xml_correct,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
    )
    .await;
    assert!(
        outcome1.corrupt.is_empty(),
        "corrupt: {:?}",
        outcome1.corrupt
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(penne::cache::load(dest.path(), "warm-msg-01").is_some());

    // Run 2 (warm cache, only the nonzero declared bytes changed).
    std::fs::remove_file(dest.path().join("warm.bin")).unwrap();
    counter.store(0, Ordering::SeqCst);
    let xml_wrong = encrypted_nzb_xml(
        kdf_input,
        "&quot;warm.bin&quot; yEnc",
        &format!(
            "<segment bytes=\"{}\" number=\"1\">warm-msg-01</segment>",
            payload.len() + 1
        ),
    );
    let outcome2 = run_download_nzb(
        &xml_wrong,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
    )
    .await;

    // Same rejection and failover shape as the cold mismatch test: the
    // segment is corrupt with SEGMENT_BYTES_MISMATCH, the provider was
    // actually consulted after the cache predicate rejected the hit, the
    // mismatching copy is not committed, and the previously valid cache
    // entry is left untouched.
    assert_eq!(outcome2.corrupt.len(), 1, "missing: {:?}", outcome2.missing);
    assert_eq!(
        outcome2.corrupt[0]
            .message_id
            .trim_start_matches('<')
            .trim_end_matches('>'),
        "warm-msg-01"
    );
    assert!(outcome2.corrupt[0].error.contains("SEGMENT_BYTES_MISMATCH"));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "the rejected cache hit must fall through to the provider fetch"
    );
    assert!(!dest.path().join("warm.bin").exists());
    assert!(penne::cache::load(dest.path(), "warm-msg-01").is_some());
}

/// Ordinary (unencrypted) downloads keep their no-declared-bytes-check cache
/// behavior: a warm hit commits even when the NZB declares different bytes.
#[tokio::test]
async fn ordinary_warm_cache_is_not_affected_by_declared_bytes() {
    let payload = b"Ordinary cached payload.";
    let encoded = encode_part(
        "plain.bin",
        payload.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload,
        128,
        None,
    );
    let article = encoded.body;

    let counter = Arc::new(AtomicUsize::new(0));
    let mut known = HashMap::new();
    known.insert("plain-msg-01".to_string(), article);
    let addr = spawn_fake_server_counting(known, counter.clone());

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="uploader@example.com" date="1774300000" subject="&quot;plain.bin&quot; yEnc (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="1">plain-msg-01</segment>
    </segments>
  </file>
</nzb>"#,
        payload.len() + 5
    );
    let dest = tempfile::tempdir().unwrap();

    // Run 1: fetch from the network with the (ignored) wrong declared bytes.
    let outcome1 =
        run_download_nzb(&xml, &[ServerTier::solo(server_entry(addr))], dest.path()).await;
    assert!(
        outcome1.corrupt.is_empty(),
        "corrupt: {:?}",
        outcome1.corrupt
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(penne::cache::load(dest.path(), "plain-msg-01").is_some());

    // Run 2: warm cache serves the hit with no network request, unchanged.
    std::fs::remove_file(dest.path().join("plain.bin")).unwrap();
    counter.store(0, Ordering::SeqCst);
    let outcome2 =
        run_download_nzb(&xml, &[ServerTier::solo(server_entry(addr))], dest.path()).await;
    assert!(
        outcome2.corrupt.is_empty(),
        "corrupt: {:?}",
        outcome2.corrupt
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "ordinary warm cache must serve the hit without the network"
    );
    assert_eq!(
        std::fs::read(dest.path().join("plain.bin")).unwrap(),
        payload,
        "ordinary cache behavior must be unchanged"
    );
}
