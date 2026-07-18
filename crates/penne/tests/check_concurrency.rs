//! Integration test mirroring `tests/concurrency.rs` (Phase 9, for
//! `download_queue`): `check_queue` must actually open `server.connections`
//! connections concurrently against one server, not drain the queue one
//! `STAT` at a time.
//!
//! The fake NNTP server below tracks how many `STAT` requests are being
//! handled *at once* (not how many total, which would be true even of a
//! sequential drain) and holds each one open briefly before answering —
//! long enough that a sequential drain could not have overlapped them.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pesto::config::ServerEntry;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use penne::check::check_queue;
use penne::queue::{DownloadQueue, QueuedFile, QueuedSegment};

/// Spawn a fake NNTP server whose `STAT` handler sleeps `delay` before
/// responding and records the peak number of `STAT` requests it was
/// handling at the same instant, across all connections.
fn spawn_slow_server(
    known: HashSet<String>,
    delay: Duration,
) -> (SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let listener_std = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener_std.set_nonblocking(true).unwrap();
    let addr = listener_std.local_addr().unwrap();
    let listener = TcpListener::from_std(listener_std).unwrap();

    let in_flight_task = in_flight.clone();
    let peak_task = peak.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let known = known.clone();
            let in_flight = in_flight_task.clone();
            let peak = peak_task.clone();
            tokio::spawn(handle_connection(stream, known, delay, in_flight, peak));
        }
    });

    (addr, in_flight, peak)
}

async fn handle_connection(
    stream: TcpStream,
    known: HashSet<String>,
    delay: Duration,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
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

        if let Some(rest) = cmd.strip_prefix("STAT ") {
            let id = rest.trim_start_matches('<').trim_end_matches('>');

            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);

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

#[tokio::test]
async fn multiple_connections_to_one_server_run_concurrently() {
    const SEGMENTS: usize = 8;
    const CONNECTIONS: usize = 4;
    const DELAY: Duration = Duration::from_millis(80);

    let mut known = HashSet::new();
    let mut segments = Vec::new();
    for i in 0..SEGMENTS {
        let id = format!("seg{i}@test");
        known.insert(id.clone());
        segments.push(QueuedSegment {
            message_id: id,
            part: i as u32 + 1,
            bytes: 5,
        });
    }
    let (addr, _in_flight, peak) = spawn_slow_server(known, DELAY);

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "movie.bin".to_string(),
            segments,
        }],
    };
    let server = ServerEntry {
        host: addr.ip().to_string(),
        port: addr.port(),
        ssl: false,
        connections: CONNECTIONS,
        username: None,
        password: None,
        retry_delay: 0,
        timeout: 5,
    };

    let started = std::time::Instant::now();
    let outcome = check_queue(&queue, &[server], 0, None).await.unwrap();
    let elapsed = started.elapsed();

    assert!(outcome.is_complete());

    // The strongest signal: more than one STAT request was ever being
    // handled at the same instant — impossible for a strictly sequential,
    // one-connection-at-a-time drain.
    assert!(
        peak.load(Ordering::SeqCst) > 1,
        "expected overlapping STAT requests, peak concurrency was {}",
        peak.load(Ordering::SeqCst)
    );

    // A secondary, coarser signal: wall-clock time. Sequential would take
    // SEGMENTS * DELAY (640ms); with CONNECTIONS=4 workers it should take
    // roughly SEGMENTS/CONNECTIONS * DELAY (~160ms). Generous slack for a
    // loaded CI machine.
    assert!(
        elapsed < DELAY * (SEGMENTS as u32) / 2,
        "took {elapsed:?}, expected well under {:?} if truly sequential",
        DELAY * SEGMENTS as u32
    );
}
