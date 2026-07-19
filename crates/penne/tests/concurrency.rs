//! Integration test for `ROADMAP.md` Phase 9: `download_queue` actually
//! opens `server.connections` connections concurrently against one server,
//! instead of draining the queue one segment at a time.
//!
//! The fake NNTP server below tracks how many `BODY` requests are being
//! handled *at once* (not how many total, which would be true even of a
//! sequential drain) and holds each one open briefly before answering —
//! long enough that a sequential drain could not have overlapped them.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use pesto::config::ServerEntry;
use pesto::yenc::{encode_part, PartSpec};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use penne::config::ServerTier;
use penne::download::download_queue;
use penne::progress::{channel, ProgressEvent};
use penne::queue::{DownloadQueue, QueuedFile, QueuedSegment};

/// Spawn a fake NNTP server whose `BODY` handler sleeps `delay` before
/// responding and records the peak number of `BODY` requests it was
/// handling at the same instant, across all connections.
fn spawn_slow_server(
    known: HashMap<String, Vec<u8>>,
    delay: Duration,
) -> (SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let addr = spawn_slow_server_sharing(known, delay, in_flight.clone(), peak.clone());
    (addr, in_flight, peak)
}

/// Same as [`spawn_slow_server`], but the caller supplies (and can share
/// across more than one spawned server) the in-flight/peak counters,
/// rather than each server getting its own — the tool
/// `two_pooled_servers_are_drained_concurrently_as_one_tier` needs below to
/// prove concurrency *across* two distinct fake servers, not just within
/// one.
fn spawn_slow_server_sharing(
    known: HashMap<String, Vec<u8>>,
    delay: Duration,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
) -> SocketAddr {
    let listener_std = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener_std.set_nonblocking(true).unwrap();
    let addr = listener_std.local_addr().unwrap();
    let listener = TcpListener::from_std(listener_std).unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let known = known.clone();
            let in_flight = in_flight.clone();
            let peak = peak.clone();
            tokio::spawn(handle_connection(stream, known, delay, in_flight, peak));
        }
    });

    addr
}

async fn handle_connection(
    stream: TcpStream,
    known: HashMap<String, Vec<u8>>,
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

        if let Some(rest) = cmd.strip_prefix("BODY ") {
            let id = rest.trim_start_matches('<').trim_end_matches('>');

            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);

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

fn yenc_body(name: &str, data: &[u8]) -> Vec<u8> {
    encode_part(
        name,
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

#[tokio::test]
async fn multiple_connections_to_one_server_run_concurrently() {
    const SEGMENTS: usize = 8;
    const CONNECTIONS: usize = 4;
    const DELAY: Duration = Duration::from_millis(80);

    let mut known = HashMap::new();
    let mut segments = Vec::new();
    for i in 0..SEGMENTS {
        let id = format!("seg{i}@test");
        known.insert(
            id.clone(),
            yenc_body("movie.bin", format!("data{i}").as_bytes()),
        );
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
    let dest_dir = tempfile::tempdir().unwrap();

    let started = std::time::Instant::now();
    let outcome = download_queue(
        &queue,
        &[ServerTier::solo(server)],
        dest_dir.path(),
        0,
        None,
    )
    .await
    .unwrap();
    let elapsed = started.elapsed();

    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());
    assert_eq!(outcome.segments.len(), SEGMENTS);

    // The strongest signal: more than one BODY request was ever being
    // handled at the same instant — impossible for a strictly sequential,
    // one-connection-at-a-time drain.
    assert!(
        peak.load(Ordering::SeqCst) > 1,
        "expected overlapping BODY requests, peak concurrency was {}",
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

/// `ROADMAP.md` Phase 15's `level`+`group` item: two *distinct* servers
/// pooled into one [`ServerTier`] are drained together as a single
/// combined worker budget, not one finishing its whole pass before the
/// other starts (which is still exactly what happens *across* tiers —
/// this only changes behavior *within* one).
///
/// Proof: both fake servers share the *same* in-flight/peak counters
/// (`spawn_slow_server_sharing`). Each server gets `CONNECTIONS_PER_SERVER
/// = 2` connections; if pooling genuinely merges their budgets into one
/// shared queue, peak concurrent in-flight requests can reach above 2 —
/// impossible if the two were (bug: still) treated as separate sequential
/// tiers, where only one server's ≤2 connections could ever be active at
/// once.
#[tokio::test]
async fn two_pooled_servers_are_drained_concurrently_as_one_tier() {
    const SEGMENTS: usize = 8;
    const CONNECTIONS_PER_SERVER: usize = 2;
    const DELAY: Duration = Duration::from_millis(80);

    let mut known = HashMap::new();
    let mut segments = Vec::new();
    for i in 0..SEGMENTS {
        let id = format!("seg{i}@test");
        known.insert(
            id.clone(),
            yenc_body("movie.bin", format!("data{i}").as_bytes()),
        );
        segments.push(QueuedSegment {
            message_id: id,
            part: i as u32 + 1,
            bytes: 5,
        });
    }

    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    // Both servers know every segment, so either one alone could in
    // principle serve the whole queue — what's under test is *scheduling*
    // (are both used at once?), not failover.
    let addr_a = spawn_slow_server_sharing(known.clone(), DELAY, in_flight.clone(), peak.clone());
    let addr_b = spawn_slow_server_sharing(known, DELAY, in_flight, peak.clone());

    let queue = DownloadQueue {
        files: vec![QueuedFile {
            name: "movie.bin".to_string(),
            segments,
        }],
    };
    let member = |addr: SocketAddr| ServerEntry {
        host: addr.ip().to_string(),
        port: addr.port(),
        ssl: false,
        connections: CONNECTIONS_PER_SERVER,
        username: None,
        password: None,
        retry_delay: 0,
        timeout: 5,
    };
    let tier = ServerTier {
        members: vec![member(addr_a), member(addr_b)],
    };
    let dest_dir = tempfile::tempdir().unwrap();

    let outcome = download_queue(&queue, &[tier], dest_dir.path(), 0, None)
        .await
        .unwrap();

    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());
    assert_eq!(outcome.segments.len(), SEGMENTS);

    assert!(
        peak.load(Ordering::SeqCst) > CONNECTIONS_PER_SERVER,
        "expected in-flight requests from both pooled servers to overlap \
         (peak > {CONNECTIONS_PER_SERVER}), peak was {}",
        peak.load(Ordering::SeqCst)
    );
}

/// Regression test mirroring `penne::check`'s identical fix: every worker
/// in a server's pass pulls from one shared queue and only stops once it's
/// empty, so a `SegmentDownloaded` event emitted only after the *whole*
/// pass returns means the progress panel sits still for the entire
/// download and then jumps straight to 100%, no matter how large the
/// release. Events must arrive while the download is still in flight.
#[tokio::test]
async fn progress_events_arrive_while_the_download_is_still_running() {
    const SEGMENTS: usize = 20;
    const CONNECTIONS: usize = 4;
    const DELAY: Duration = Duration::from_millis(50);

    let mut known = HashMap::new();
    let mut segments = Vec::new();
    for i in 0..SEGMENTS {
        let id = format!("seg{i}@test");
        known.insert(
            id.clone(),
            yenc_body("movie.bin", format!("d{i}").as_bytes()),
        );
        segments.push(QueuedSegment {
            message_id: id,
            part: i as u32 + 1,
            bytes: 5,
        });
    }
    let (addr, _in_flight, _peak) = spawn_slow_server(known, DELAY);

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
    let dest_dir = tempfile::tempdir().unwrap();
    let dest_path = dest_dir.path().to_path_buf();

    let (tx, mut rx) = channel();
    let handle = tokio::spawn(async move {
        download_queue(&queue, &[ServerTier::solo(server)], &dest_path, 0, Some(tx)).await
    });

    // The `Started` event fires immediately; skip past it to the first
    // real per-segment event.
    loop {
        let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("timed out waiting for the first progress event — events aren't being streamed")
            .expect("channel closed before any per-segment event arrived");
        if matches!(ev, ProgressEvent::Started { .. }) {
            continue;
        }
        assert!(
            matches!(ev, ProgressEvent::SegmentDownloaded { .. }),
            "expected a SegmentDownloaded event, got {ev:?}"
        );
        break;
    }

    // Total expected wall time is roughly SEGMENTS/CONNECTIONS * DELAY =
    // 250ms. Confirming the download hasn't already finished by the time
    // its first per-segment event was consumed proves events are streamed
    // during the run rather than dumped all at once right before
    // `download_queue` returns.
    assert!(
        !handle.is_finished(),
        "download_queue already finished by the time its first SegmentDownloaded event was \
         consumed — events are batched at the end instead of streamed"
    );

    let outcome = handle.await.unwrap().unwrap();
    assert_eq!(outcome.segments.len(), SEGMENTS);
}

/// Companion regression test for a real report against `penne::check`'s
/// identical fix: a release with many missing articles against a single
/// server must also stream `SegmentMissing` events per-item on that (the
/// last) server, not batch them into the post-loop — otherwise a
/// mostly-missing download would sit at 0% for the whole run too.
#[tokio::test]
async fn missing_progress_events_arrive_while_the_download_is_still_running() {
    const SEGMENTS: usize = 20;
    const CONNECTIONS: usize = 4;
    const DELAY: Duration = Duration::from_millis(50);

    // No known bodies at all — every BODY request comes back 430.
    let (addr, _in_flight, _peak) = spawn_slow_server(HashMap::new(), DELAY);

    let mut segments = Vec::new();
    for i in 0..SEGMENTS {
        segments.push(QueuedSegment {
            message_id: format!("seg{i}@test"),
            part: i as u32 + 1,
            bytes: 5,
        });
    }
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
    let dest_dir = tempfile::tempdir().unwrap();
    let dest_path = dest_dir.path().to_path_buf();

    let (tx, mut rx) = channel();
    let handle = tokio::spawn(async move {
        download_queue(&queue, &[ServerTier::solo(server)], &dest_path, 0, Some(tx)).await
    });

    loop {
        let ev = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect(
                "timed out waiting for the first missing-progress event — \
                 a mostly-missing download isn't streaming progress either",
            )
            .expect("channel closed before any per-segment event arrived");
        if matches!(ev, ProgressEvent::Started { .. }) {
            continue;
        }
        assert!(
            matches!(ev, ProgressEvent::SegmentMissing { .. }),
            "expected a SegmentMissing event, got {ev:?}"
        );
        break;
    }

    assert!(
        !handle.is_finished(),
        "download_queue already finished by the time its first SegmentMissing event was \
         consumed — missing events are still batched at the end instead of streamed"
    );

    let outcome = handle.await.unwrap().unwrap();
    assert_eq!(outcome.missing.len(), SEGMENTS);
}

/// Regression test for the streaming-assembly phase: a file whose segments
/// all resolve early must be written to disk (and its `FileAssembled` event
/// emitted) right away, while other files in the same queue are still being
/// fetched — not held in memory until the whole multi-file queue finishes.
#[tokio::test]
async fn a_file_that_finishes_early_is_assembled_before_the_rest_of_the_queue() {
    const SLOW_SEGMENTS: usize = 20;
    const CONNECTIONS: usize = 4;
    const DELAY: Duration = Duration::from_millis(50);

    let mut known = HashMap::new();
    known.insert(
        "fast0@test".to_string(),
        yenc_body("fast.bin", b"fast-data"),
    );
    let mut slow_segments = Vec::new();
    for i in 0..SLOW_SEGMENTS {
        let id = format!("slow{i}@test");
        known.insert(
            id.clone(),
            yenc_body("slow.bin", format!("d{i}").as_bytes()),
        );
        slow_segments.push(QueuedSegment {
            message_id: id,
            part: i as u32 + 1,
            bytes: 5,
        });
    }
    let (addr, _in_flight, _peak) = spawn_slow_server(known, DELAY);

    let queue = DownloadQueue {
        files: vec![
            QueuedFile {
                name: "fast.bin".to_string(),
                segments: vec![QueuedSegment {
                    message_id: "fast0@test".to_string(),
                    part: 1,
                    bytes: 5,
                }],
            },
            QueuedFile {
                name: "slow.bin".to_string(),
                segments: slow_segments,
            },
        ],
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
    let dest_dir = tempfile::tempdir().unwrap();
    let dest_path = dest_dir.path().to_path_buf();

    let (tx, mut rx) = channel();
    let handle = tokio::spawn(async move {
        download_queue(&queue, &[ServerTier::solo(server)], &dest_path, 0, Some(tx)).await
    });

    loop {
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for fast.bin to be assembled")
            .expect("channel closed before fast.bin was assembled");
        if let ProgressEvent::FileAssembled { file_name } = &ev {
            assert_eq!(file_name, "fast.bin");
            break;
        }
    }

    // `slow.bin` has far more segments than `fast.bin` and shares the same
    // per-request delay, so the whole download can't possibly be done yet —
    // proving `fast.bin` was assembled mid-run, not batched at the end.
    assert!(
        !handle.is_finished(),
        "download_queue already finished by the time fast.bin's FileAssembled event was \
         consumed — assembly is still batched at the end instead of streamed per-file"
    );
    assert!(
        dest_dir.path().join("fast.bin").exists(),
        "fast.bin should already be on disk while slow.bin is still downloading"
    );

    let outcome = handle.await.unwrap().unwrap();
    assert_eq!(outcome.segments.len(), SLOW_SEGMENTS + 1);
    assert!(outcome.assembled.contains_key("fast.bin"));
    assert!(outcome.assembled.contains_key("slow.bin"));
    assert!(dest_dir.path().join("slow.bin").exists());
}
