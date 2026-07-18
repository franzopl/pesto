//! Integration test: `penne::check::check_queue` against a local, in-process
//! fake NNTP server that only understands `STAT` and `QUIT` (loopback only —
//! no real Usenet server). Mirrors the async mock-server pattern used by
//! `tests/download_with_failover.rs`.

use std::collections::HashSet;
use std::net::SocketAddr;

use pesto::config::ServerEntry;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use penne::check::check_queue;
use penne::queue::{DownloadQueue, QueuedFile, QueuedSegment};

/// Spawn a fake NNTP server that only understands `STAT` and `QUIT`.
/// `known` is the set of bare Message-IDs that get a `223` (present); any
/// other Message-ID gets a `430` (not found).
fn spawn_fake_server(known: HashSet<&'static str>) -> SocketAddr {
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

async fn handle_connection(stream: TcpStream, known: HashSet<&'static str>) {
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

        if let Some(rest) = cmd.strip_prefix("STAT ") {
            let id = rest.trim_start_matches('<').trim_end_matches('>');
            let resp = if known.contains(id) {
                format!("223 0 <{id}>\r\n")
            } else {
                "430 No such article\r\n".to_string()
            };
            if w.write_all(resp.as_bytes()).await.is_err() {
                return;
            }
        } else if cmd == "QUIT" {
            let _ = w.write_all(b"205 bye\r\n").await;
            return;
        } else if w.write_all(b"500 unknown command\r\n").await.is_err() {
            return;
        }
    }
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

fn queue_with(files: &[(&str, &[&str])]) -> DownloadQueue {
    DownloadQueue {
        files: files
            .iter()
            .map(|(name, ids)| QueuedFile {
                name: name.to_string(),
                segments: ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| QueuedSegment {
                        message_id: id.to_string(),
                        part: (i + 1) as u32,
                        bytes: 4,
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[tokio::test]
async fn every_segment_present_reports_complete() {
    let known: HashSet<&str> = ["a1@x", "a2@x", "b1@x"].into_iter().collect();
    let addr = spawn_fake_server(known);

    let queue = queue_with(&[("movie.bin", &["a1@x", "a2@x"]), ("movie.par2", &["b1@x"])]);
    let outcome = check_queue(&queue, &[server_entry(addr)], 0).await.unwrap();

    assert!(outcome.is_complete());
    assert!(outcome.missing.is_empty());
    assert_eq!(outcome.files.len(), 2);
    assert_eq!(outcome.files[0].name, "movie.bin");
    assert!(outcome.files[0].is_complete());
    assert_eq!(outcome.files[0].present_segments, 2);
    assert_eq!(outcome.files[1].name, "movie.par2");
    assert!(outcome.files[1].is_complete());
}

#[tokio::test]
async fn missing_segments_are_reported_per_file_and_overall() {
    // Only a1@x exists; a2@x (part of movie.bin) and b1@x (movie.par2, the
    // only segment of that file) are both gone.
    let known: HashSet<&str> = ["a1@x"].into_iter().collect();
    let addr = spawn_fake_server(known);

    let queue = queue_with(&[("movie.bin", &["a1@x", "a2@x"]), ("movie.par2", &["b1@x"])]);
    let outcome = check_queue(&queue, &[server_entry(addr)], 0).await.unwrap();

    assert!(!outcome.is_complete());
    assert_eq!(outcome.missing.len(), 2);

    let movie = &outcome.files[0];
    assert_eq!(movie.present_segments, 1);
    assert_eq!(movie.total_segments, 2);
    assert!(!movie.is_complete());

    let par2 = &outcome.files[1];
    assert_eq!(par2.present_segments, 0);
    assert!(!par2.is_complete());
}

#[tokio::test]
async fn falls_back_to_backup_server_for_segments_the_primary_lacks() {
    let primary = spawn_fake_server(HashSet::new()); // knows nothing
    let backup_known: HashSet<&str> = ["a1@x"].into_iter().collect();
    let backup = spawn_fake_server(backup_known);

    let queue = queue_with(&[("movie.bin", &["a1@x"])]);
    let servers = vec![server_entry(primary), server_entry(backup)];
    let outcome = check_queue(&queue, &servers, 0).await.unwrap();

    assert!(outcome.is_complete());
    assert_eq!(outcome.files[0].present_segments, 1);
}

#[tokio::test]
async fn bytes_used_reflects_the_exact_wire_cost_of_the_check() {
    let known: HashSet<&str> = ["a1@x"].into_iter().collect();
    let addr = spawn_fake_server(known);

    let queue = queue_with(&[("movie.bin", &["a1@x"])]);
    let outcome = check_queue(&queue, &[server_entry(addr)], 0).await.unwrap();

    // One connection, one segment checked: the server's unsolicited
    // greeting ("200 mock ready\r\n"), the "STAT <a1@x>\r\n" command
    // (14 bytes written), and its "223 0 <a1@x>\r\n" response — nothing
    // more, proving `--stat` really does stay this cheap.
    let greeting = "200 mock ready\r\n".len() as u64;
    let stat_written = "STAT <a1@x>".len() as u64 + 2;
    let stat_response_read = "223 0 <a1@x>\r\n".len() as u64;
    assert_eq!(
        outcome.bytes_used,
        greeting + stat_written + stat_response_read
    );
}

#[tokio::test]
async fn no_server_has_it_reports_missing_after_trying_every_server() {
    let primary = spawn_fake_server(HashSet::new());
    let backup = spawn_fake_server(HashSet::new());

    let queue = queue_with(&[("movie.bin", &["a1@x"])]);
    let servers = vec![server_entry(primary), server_entry(backup)];
    let outcome = check_queue(&queue, &servers, 0).await.unwrap();

    assert!(!outcome.is_complete());
    assert_eq!(outcome.missing.len(), 1);
    assert_eq!(outcome.missing[0].file_name, "movie.bin");
}
