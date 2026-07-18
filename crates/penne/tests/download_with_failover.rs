//! Integration test: `penne::download::download_queue` against a local,
//! in-process fake NNTP server (loopback only — no real Usenet server).
//! Mirrors the mock-server pattern `pesto`'s own integration tests already
//! use (see `crates/pesto/tests/server_substituted_message_id.rs`), adapted
//! to `tokio` since `penne`'s client is async.

use std::collections::HashMap;
use std::net::SocketAddr;

use pesto::config::ServerEntry;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};

use penne::download::download_queue;
use penne::queue::{DownloadQueue, QueuedFile, QueuedSegment};

const RAW_BODY: &[u8] = b"hello\r\n.world\r\ndone\r\n";

/// Spawn a fake NNTP server that only understands `BODY` and `QUIT`. `known`
/// maps bare Message-IDs to the raw bytes the client should get back; the
/// server dot-stuffs them on the wire itself, so a successful fetch proves
/// the client undoes dot-stuffing correctly over a real TCP round-trip.
fn spawn_fake_server(known: HashMap<&'static str, &'static [u8]>) -> SocketAddr {
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

async fn handle_connection(stream: TcpStream, known: HashMap<&'static str, &'static [u8]>) {
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

/// Write `body` to `w`, doubling any line-leading `.` per RFC 3977 §3.1.1.
/// Assumes every line in `body` ends with `\n` (true for the fixed test
/// bodies below), so line boundaries on the wire always land on a `\n`.
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

#[tokio::test]
async fn fetches_from_the_only_configured_server() {
    let mut known = HashMap::new();
    known.insert("art1@test", RAW_BODY);
    let addr = spawn_fake_server(known);

    let queue = queue_with_one_segment("art1@test");
    let servers = vec![server_entry(addr)];

    let outcome = download_queue(&queue, &servers, None).await.unwrap();
    assert!(outcome.missing.is_empty());
    assert_eq!(outcome.bodies.get("art1@test").unwrap(), RAW_BODY);
}

#[tokio::test]
async fn falls_back_to_backup_when_primary_is_missing() {
    let primary = spawn_fake_server(HashMap::new()); // knows nothing
    let mut backup_known = HashMap::new();
    backup_known.insert("art1@test", RAW_BODY);
    let backup = spawn_fake_server(backup_known);

    let queue = queue_with_one_segment("art1@test");
    let servers = vec![server_entry(primary), server_entry(backup)];

    let outcome = download_queue(&queue, &servers, None).await.unwrap();
    assert!(outcome.missing.is_empty());
    assert_eq!(outcome.bodies.get("art1@test").unwrap(), RAW_BODY);
}

#[tokio::test]
async fn records_missing_when_no_server_has_it() {
    let a = spawn_fake_server(HashMap::new());
    let b = spawn_fake_server(HashMap::new());

    let queue = queue_with_one_segment("ghost@test");
    let servers = vec![server_entry(a), server_entry(b)];

    let outcome = download_queue(&queue, &servers, None).await.unwrap();
    assert!(outcome.bodies.is_empty());
    assert_eq!(outcome.missing.len(), 1);
    assert_eq!(outcome.missing[0].message_id, "ghost@test");
    assert_eq!(outcome.missing[0].file_name, "movie.bin");
}
