//! Password-override integration tests for Penne encrypted downloads
//! (PD12-01 / Q1(a)).
//!
//! Split out of `download_encryption.rs` to keep that suite within its
//! source-size baseline. An explicitly supplied transport password override
//! ahead of `<meta type="password">` downloads when the meta password is
//! absent or wrong, while the `yenc_encrypted` marker remains the only
//! gate — without it the override is never used for transport.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;

use pesto::config::ServerEntry;
use pesto::crypto::EncryptionSession;
use pesto::yenc::PartSpec;
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};

mod support;
use penne::config::ServerTier;
use penne::download::{download_queue_with_decryptor, DownloadOutcome};
use penne::nzb::{download_decryptor, download_decryptor_with_password_override, load};
use penne::queue::build;
use support::{fixture_kdf_input, other_kdf_input};

/// Spawn a fake NNTP server that only understands `BODY` and `QUIT` — the
/// commands `download_queue_with_decryptor` actually sends.
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

fn write_temp_nzb(xml: &str) -> NamedTempFile {
    let mut temp = NamedTempFile::new().expect("failed to create temp file");
    temp.write_all(xml.as_bytes()).expect("failed to write XML");
    temp
}

fn encrypted_nzb_xml(password: &str, file_subject: &str, segments: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
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

/// Encrypt one article segment with the supplied segment index (Body Std
/// v1.2 §8: every article in a fixture uses a distinct index).
fn encode_article(
    session: &Arc<EncryptionSession>,
    file_name: &str,
    file_size: u64,
    spec: PartSpec,
    payload: &[u8],
    segment_index: u32,
) -> Vec<u8> {
    let uploader = pesto::crypto::UploadEncryptionAdapter::new(session.clone());
    let mut body = Vec::new();
    let encoded = uploader
        .encode_article(
            file_name,
            file_size,
            spec,
            payload,
            128,
            segment_index,
            &mut body,
        )
        .expect("encode article failed");
    encoded.body
}

fn single_part() -> PartSpec {
    PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    }
}

/// Q1(a) (2026-10-16 PD12-01): an explicitly supplied transport password
/// override ahead of `<meta type="password">` downloads when the meta
/// password is absent or wrong, while the `yenc_encrypted` marker remains
/// the only gate — without it the override is never used for transport.
#[tokio::test]
async fn password_override_downloads_with_absent_or_wrong_meta_password() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x38u8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"Payload decrypted only with the override password.";
    let encoded = encode_article(
        &session,
        "override.bin",
        payload.len() as u64,
        single_part(),
        payload,
        1,
    );
    let segment = format!(
        "<segment bytes=\"{}\" number=\"1\">override-msg-01@test</segment>",
        payload.len()
    );
    async fn run(
        xml: &str,
        tiers: &[ServerTier],
        dest: &std::path::Path,
        kdf_input: &str,
    ) -> DownloadOutcome {
        let nzb_file = write_temp_nzb(xml);
        let parsed = load(nzb_file.path()).unwrap();
        let queue = build(&parsed);
        let decryptor =
            download_decryptor_with_password_override(&parsed.meta, Some(kdf_input)).unwrap();
        download_queue_with_decryptor(&queue, tiers, dest, 0, None, decryptor)
            .await
            .unwrap()
    }

    // (a) Meta password absent: the override supplies the transport password.
    let xml_absent = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;override.bin&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      {segment}
    </segments>
  </file>
</nzb>"#
    );
    let mut known = HashMap::new();
    known.insert("override-msg-01@test".to_string(), encoded.clone());
    let addr = spawn_fake_server(known);
    let dest = tempfile::tempdir().unwrap();
    let outcome = run(
        &xml_absent,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
        kdf_input,
    )
    .await;
    assert!(outcome.missing.is_empty(), "missing: {:?}", outcome.missing);
    assert!(outcome.corrupt.is_empty(), "corrupt: {:?}", outcome.corrupt);
    assert_eq!(
        std::fs::read(dest.path().join("override.bin")).unwrap(),
        payload
    );

    // (b) Meta password wrong: the override ahead of it wins.
    let xml_wrong = encrypted_nzb_xml(other_kdf_input(), "&quot;override.bin&quot; yEnc", &segment);
    let mut known = HashMap::new();
    known.insert("override-msg-01@test".to_string(), encoded);
    let addr = spawn_fake_server(known);
    let dest = tempfile::tempdir().unwrap();
    let outcome = run(
        &xml_wrong,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
        kdf_input,
    )
    .await;
    assert!(outcome.missing.is_empty(), "missing: {:?}", outcome.missing);
    assert!(outcome.corrupt.is_empty(), "corrupt: {:?}", outcome.corrupt);
    assert_eq!(
        std::fs::read(dest.path().join("override.bin")).unwrap(),
        payload
    );
}

/// Q1(a) gate: no `yenc_encrypted` marker ⇒ the override never constructs a
/// transport decryptor (the ordinary NZB is unaffected; the value keeps only
/// its downstream archive role, exercised by the CLI `--password` plumbing).
#[test]
fn password_override_is_ignored_without_encrypted_marker() {
    let meta = pesto::nzb::NzbMeta {
        yenc_encrypted: false,
        password: Some(fixture_kdf_input().to_string()),
        ..Default::default()
    };
    assert!(
        download_decryptor_with_password_override(&meta, Some(fixture_kdf_input()))
            .unwrap()
            .is_none()
    );
    assert!(download_decryptor(&meta).unwrap().is_none());
}
