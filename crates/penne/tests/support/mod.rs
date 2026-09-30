//! Shared test-only PAR2 fixture builder, used by more than one integration
//! test file (hence `tests/support/mod.rs`, not a top-level `tests/*.rs`,
//! which Cargo would otherwise treat as its own test binary).
//!
//! Drives the real PAR2 encoder and packet writers via `pesto::par2`'s fully
//! public API, adapted from `crates/parmesan/src/test_support.rs` (which is
//! `pub(crate)` there and not reachable from another crate). This proves
//! `penne`'s PAR2 integration against genuine on-disk PAR2 bytes rather than
//! a fake in-memory `RecoverySet`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pesto::config::ServerEntry;
use pesto::par2::encoder::{FileHasher, RecoveryEncoder};
use pesto::par2::packet;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};

pub struct FixtureFile {
    pub name: &'static str,
    pub data: Vec<u8>,
}

/// Build a small PAR2 recovery set (`base.par2` index + one recovery
/// volume, when `recovery_count > 0`) under a fresh temp directory. Returns
/// the directory path; the caller is responsible for removing it when done.
pub fn build_fixture_set(
    files: &[FixtureFile],
    slice_size: usize,
    recovery_count: usize,
) -> PathBuf {
    let dir = tempfile::tempdir().unwrap().keep();

    for f in files {
        std::fs::write(dir.join(f.name), &f.data).unwrap();
    }

    let total_slices: usize = files
        .iter()
        .map(|f| f.data.len().div_ceil(slice_size))
        .sum();

    let mut enc =
        RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count).with_checksums();

    let mut hashes = Vec::new();
    let mut slice_counts = Vec::new();
    for f in files {
        let mut hasher = FileHasher::new();
        let n_slices = f.data.len().div_ceil(slice_size);
        let mut pos = 0usize;
        for _ in 0..n_slices {
            let end = (pos + slice_size).min(f.data.len());
            let chunk = &f.data[pos..end];
            hasher.update(chunk);
            let mut padded = vec![0u8; slice_size];
            padded[..chunk.len()].copy_from_slice(chunk);
            enc.add_slice(padded);
            pos = end;
        }
        hashes.push(hasher.finish());
        slice_counts.push(n_slices);
    }

    let (recovery_slices, all_checksums) = enc.finish();

    let file_ids: Vec<[u8; 16]> = files
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let h = &hashes[idx];
            packet::compute_file_id(&h.md5_16k, h.length, f.name)
        })
        .collect();

    let main_b = packet::main_body(slice_size as u64, &file_ids);
    let rsid = packet::recovery_set_id(&main_b);

    let mut index_bytes = Vec::new();
    index_bytes.extend(packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main_b));
    index_bytes.extend(packet::serialize_packet(
        &rsid,
        &packet::TYPE_CREATOR,
        &packet::creator_body("penne-tests"),
    ));

    let mut cursor = 0usize;
    for (idx, f) in files.iter().enumerate() {
        let fid = &file_ids[idx];
        let h = &hashes[idx];
        index_bytes.extend(packet::serialize_packet(
            &rsid,
            &packet::TYPE_FILE_DESC,
            &packet::file_description_body(fid, &h.md5_full, &h.md5_16k, h.length, f.name),
        ));
        let n = slice_counts[idx];
        let slices = &all_checksums[cursor..cursor + n];
        cursor += n;
        index_bytes.extend(packet::serialize_packet(
            &rsid,
            &packet::TYPE_IFSC,
            &packet::ifsc_body(fid, slices),
        ));
    }

    std::fs::write(dir.join("base.par2"), &index_bytes).unwrap();

    if !recovery_slices.is_empty() {
        let mut vol_bytes = index_bytes.clone();
        for slice in &recovery_slices {
            vol_bytes.extend(packet::serialize_packet(
                &rsid,
                &packet::TYPE_RECOVERY,
                &packet::recovery_body(slice.exponent, &slice.data),
            ));
        }
        let vol_path = dir.join(format!("base.vol000+{:03}.par2", recovery_slices.len()));
        std::fs::write(&vol_path, &vol_bytes).unwrap();
    }

    dir
}

/// Spawn a loopback mock NNTP server for integration testing.
pub fn spawn_mock_nntp_server(
    known: HashMap<String, Vec<u8>>,
    request_counter: Option<Arc<AtomicUsize>>,
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
            let known = known.clone();
            let counter = request_counter.clone();
            tokio::spawn(handle_connection(stream, known, counter));
        }
    });

    addr
}

async fn handle_connection(
    stream: TcpStream,
    known: HashMap<String, Vec<u8>>,
    counter: Option<Arc<AtomicUsize>>,
) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    if w.write_all(b"200 news.mock ready\r\n").await.is_err() {
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

        if cmd == "MODE READER" {
            if w.write_all(b"200 reader\r\n").await.is_err() {
                return;
            }
        } else if cmd.starts_with("GROUP ") {
            if w.write_all(b"211 0 0 0 test\r\n").await.is_err() {
                return;
            }
        } else if let Some(rest) = cmd.strip_prefix("STAT ") {
            let id = rest.trim().trim_start_matches('<').trim_end_matches('>');
            if known.contains_key(id) {
                let resp = format!("223 0 <{id}>\r\n");
                if w.write_all(resp.as_bytes()).await.is_err() {
                    return;
                }
            } else if w.write_all(b"430 no such article\r\n").await.is_err() {
                return;
            }
        } else if let Some(rest) = cmd.strip_prefix("BODY ") {
            if let Some(ref c) = counter {
                c.fetch_add(1, Ordering::SeqCst);
            }
            let id = rest.trim().trim_start_matches('<').trim_end_matches('>');
            match known.get(id) {
                Some(body) => {
                    let header = format!("222 0 <{id}> body\r\n");
                    if w.write_all(header.as_bytes()).await.is_err()
                        || write_dot_stuffed(&mut w, body).await.is_err()
                        || w.write_all(if body.ends_with(b"\r\n") {
                            b".\r\n"
                        } else {
                            b"\r\n.\r\n"
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                None => {
                    if w.write_all(b"430 no such article\r\n").await.is_err() {
                        return;
                    }
                }
            }
        } else if let Some(rest) = cmd.strip_prefix("ARTICLE ") {
            if let Some(ref c) = counter {
                c.fetch_add(1, Ordering::SeqCst);
            }
            let id = rest.trim().trim_start_matches('<').trim_end_matches('>');
            match known.get(id) {
                Some(body) => {
                    let header = format!("220 0 <{id}> article\r\n");
                    if w.write_all(header.as_bytes()).await.is_err()
                        || write_dot_stuffed(&mut w, body).await.is_err()
                        || w.write_all(if body.ends_with(b"\r\n") {
                            b".\r\n"
                        } else {
                            b"\r\n.\r\n"
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                None => {
                    if w.write_all(b"430 no such article\r\n").await.is_err() {
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

pub fn server_entry(addr: SocketAddr) -> ServerEntry {
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
