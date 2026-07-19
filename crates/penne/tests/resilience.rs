//! Integration tests for `ROADMAP.md` Phase 8: resume via the segment cache
//! ([`penne::cache`]) and per-segment retry/backoff
//! ([`penne::download::download_queue`]'s `retries` parameter).

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use pesto::config::ServerEntry;
use pesto::yenc::{encode_part, PartSpec};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream as TokioTcpStream};

use penne::config::ServerTier;
use penne::download::download_queue;
use penne::queue::{DownloadQueue, QueuedFile, QueuedSegment};

fn yenc_body(data: &[u8]) -> Vec<u8> {
    encode_part(
        "movie.bin",
        data.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        data,
        128,
        None,
    )
    .body
}

fn server_entry(addr: SocketAddr) -> ServerEntry {
    ServerEntry {
        host: addr.ip().to_string(),
        port: addr.port(),
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        retry_delay: 0, // tests run fast; a real deployment would set this higher
        timeout: 5,
    }
}

fn queue_with_one_segment(message_id: &str) -> DownloadQueue {
    DownloadQueue {
        files: vec![QueuedFile {
            name: "movie.bin".to_string(),
            segments: vec![QueuedSegment {
                message_id: message_id.to_string(),
                part: 1,
                bytes: 4,
            }],
        }],
    }
}

// ── A normal fake NNTP server (understands BODY/QUIT only) ─────────────────

async fn spawn_fake_server(known: HashMap<&'static str, Vec<u8>>) -> SocketAddr {
    let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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

async fn handle_connection(stream: TokioTcpStream, known: HashMap<&'static str, Vec<u8>>) {
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

async fn write_dot_stuffed(
    w: &mut tokio::net::tcp::OwnedWriteHalf,
    body: &[u8],
) -> std::io::Result<()> {
    for line in body.split_inclusive(|&b| b == b'\n') {
        if line.starts_with(b".") {
            w.write_all(b".").await?;
        }
        w.write_all(line).await?;
    }
    Ok(())
}

// ── A server that drops the connection outright for its first N accepts,
//    then behaves normally — simulates a transient network failure. ───────

fn spawn_flaky_then_ok_server(
    known: HashMap<&'static str, Vec<u8>>,
    fail_first: u32,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let attempts = Arc::new(AtomicU32::new(0));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            if n < fail_first {
                // Close immediately: the client won't even get a greeting,
                // so `Connection::connect` fails with "connection closed by
                // server" — a transient-looking failure to retry past.
                drop(stream);
                continue;
            }
            let known = known.clone();
            std::thread::spawn(move || handle_connection_sync(stream, known));
        }
    });

    addr
}

fn handle_connection_sync(stream: TcpStream, known: HashMap<&'static str, Vec<u8>>) {
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = StdBufReader::new(stream);
    if writer.write_all(b"200 mock ready\r\n").is_err() {
        return;
    }
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let cmd = line.trim_end();
        if let Some(rest) = cmd.strip_prefix("BODY ") {
            let id = rest.trim_start_matches('<').trim_end_matches('>');
            match known.get(id) {
                Some(body) => {
                    let header = format!("222 0 <{id}> body\r\n");
                    if writer.write_all(header.as_bytes()).is_err() {
                        return;
                    }
                    for chunk in body.split_inclusive(|&b| b == b'\n') {
                        if chunk.starts_with(b".") && writer.write_all(b".").is_err() {
                            return;
                        }
                        if writer.write_all(chunk).is_err() {
                            return;
                        }
                    }
                    if writer.write_all(b".\r\n").is_err() {
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
}

// ── Resume (cache) ──────────────────────────────────────────────────────────

#[tokio::test]
async fn resume_skips_the_network_for_a_segment_already_cached() {
    let data = b"hello world".to_vec();
    let dest_dir = tempfile::tempdir().unwrap();

    // Simulate a previous, interrupted run having already fetched and
    // cached this segment's body.
    penne::cache::store(dest_dir.path(), "art1@test", &yenc_body(&data)).unwrap();

    // The only configured server knows *nothing* — if download_queue tried
    // the network for this segment it would come back missing.
    let addr = spawn_fake_server(HashMap::new()).await;
    let servers = vec![ServerTier::solo(server_entry(addr))];
    let queue = queue_with_one_segment("art1@test");

    let outcome = download_queue(&queue, &servers, dest_dir.path(), 0, None)
        .await
        .unwrap();
    assert!(outcome.missing.is_empty());
    assert_eq!(outcome.segments.get("art1@test").unwrap().data, data);
}

#[tokio::test]
async fn a_freshly_fetched_segment_is_cached_for_a_future_run() {
    let data = b"hello world".to_vec();
    let mut known = HashMap::new();
    known.insert("art1@test", yenc_body(&data));
    let addr = spawn_fake_server(known).await;

    let dest_dir = tempfile::tempdir().unwrap();
    let servers = vec![ServerTier::solo(server_entry(addr))];
    let queue = queue_with_one_segment("art1@test");

    assert!(penne::cache::load(dest_dir.path(), "art1@test").is_none());
    download_queue(&queue, &servers, dest_dir.path(), 0, None)
        .await
        .unwrap();
    assert!(penne::cache::load(dest_dir.path(), "art1@test").is_some());
}

// ── Retry / backoff ─────────────────────────────────────────────────────────

#[tokio::test]
async fn retries_recover_from_a_transient_connection_failure() {
    let data = b"hello world".to_vec();
    let mut known = HashMap::new();
    known.insert("art1@test", yenc_body(&data));
    // First two connection attempts get dropped; the third succeeds.
    let addr = spawn_flaky_then_ok_server(known, 2);

    let dest_dir = tempfile::tempdir().unwrap();
    let servers = vec![ServerTier::solo(server_entry(addr))];
    let queue = queue_with_one_segment("art1@test");

    let outcome = download_queue(&queue, &servers, dest_dir.path(), 3, None)
        .await
        .unwrap();
    assert!(outcome.missing.is_empty(), "{outcome:?}");
    assert_eq!(outcome.segments.get("art1@test").unwrap().data, data);
}

#[tokio::test]
async fn exhausting_retries_gives_up_and_reports_missing() {
    // Every connection attempt is dropped; with only 1 retry the segment
    // must be reported missing rather than hanging or erroring the whole run.
    let addr = spawn_flaky_then_ok_server(HashMap::new(), u32::MAX);

    let dest_dir = tempfile::tempdir().unwrap();
    let servers = vec![ServerTier::solo(server_entry(addr))];
    let queue = queue_with_one_segment("art1@test");

    let outcome = download_queue(&queue, &servers, dest_dir.path(), 1, None)
        .await
        .unwrap();
    assert_eq!(outcome.missing.len(), 1);
    assert_eq!(outcome.missing[0].message_id, "art1@test");
}
