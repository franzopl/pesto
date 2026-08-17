//! Regression guard for real pause/resume in the poster (ROADMAP.new.md
//! Phase 2, "Real pause/resume in the poster"): `poster::post_files_inner`'s
//! `external_pause` flag must actually suspend workers at a segment-batch
//! boundary (no further progress while paused) without tearing down their
//! NNTP connections, and cancelling while paused must stay about as
//! responsive as cancelling normally — not gated behind the multi-second
//! keepalive poll interval used for idle-but-unpaused connections.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files_inner;
use pesto::progress::ProgressEvent;
use pesto::walk::expand_inputs;

fn spawn_counting_server(accepted: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            accepted.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || handle_connection(stream));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    if writer.write_all(b"200 pesto mock ready\r\n").is_err() {
        return;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end().to_string();

        if command == "POST" {
            if writer.write_all(b"340 send article\r\n").is_err() {
                return;
            }
            let mut raw = Vec::new();
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
            }
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if command.starts_with("MODE READER") {
            if writer.write_all(b"200 reader mode\r\n").is_err() {
                return;
            }
        } else if command == "QUIT" {
            let _ = writer.write_all(b"205 bye\r\n");
            return;
        } else if writer.write_all(b"500 unknown command\r\n").is_err() {
            return;
        }
    }
}

const ARTICLE_SIZE: usize = 4096;

fn test_config(port: u16, connections: usize) -> Config {
    Config {
        host: "127.0.0.1".to_string(),
        port,
        ssl: false,
        connections,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: ARTICLE_SIZE,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: None,
        memory_limit: None,
        par2_temp_dir: None,
        compress_temp_dir: None,
        par2_only: false,
        par2_before_upload: false,
        threads: 0,
        simd: pesto::par2::SimdPath::Auto,
        extra_servers: vec![],
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        compress_volume_size: None,
        nzb_title: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: vec![],
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
        tvdb_kind: None,
        mal_id: None,
        indexer_url: None,
        indexer_api_key: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: false,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        file_counter: false,
        message_id_domain: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: false,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 5,
        check_retries: 2,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        check_recover_percent: 15,
        check_recover_max: 0,
        pipeline_depth: 1,
        keepalive_interval: 0,
    }
}

fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len as u64)
        .map(|i| {
            let mut z = i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (seed as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z >> 33) as u8
        })
        .collect()
}

/// Pausing mid-run must stop progress without dropping the connections, and
/// resuming must pick back up without a reconnect.
#[tokio::test(flavor = "multi_thread")]
async fn pause_stalls_progress_then_resume_completes_without_reconnecting() {
    const CONNECTIONS: usize = 2;
    const SEGMENTS: usize = 12;

    let accepted = Arc::new(AtomicUsize::new(0));
    let addr = spawn_counting_server(accepted.clone());

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, content(1, ARTICLE_SIZE * SEGMENTS)).unwrap();

    let config = test_config(addr.port(), CONNECTIONS);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();

    let pause = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let run = {
        let pause = pause.clone();
        tokio::spawn(async move {
            post_files_inner(
                &config,
                &inputs,
                Some(tx),
                None,
                None,
                None,
                None,
                Some(pause),
            )
            .await
        })
    };

    // Wait for the first confirmed segment, then pause.
    loop {
        match rx.recv().await {
            Some(ProgressEvent::SegmentDone { .. }) => break,
            Some(_) => continue,
            None => panic!("progress channel closed before any segment completed"),
        }
    }
    pause.store(true, Ordering::Relaxed);

    // Drain whatever was already in flight, then take two snapshots of
    // completed-segment count spaced apart — while genuinely paused, the
    // second snapshot must equal the first (no progress happens in between).
    let mut done = 0usize;
    let count_for = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<ProgressEvent>,
                     done: &mut usize| {
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, ProgressEvent::SegmentDone { .. }) {
                *done += 1;
            }
        }
    };
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    count_for(&mut rx, &mut done);
    let snapshot_a = done;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    count_for(&mut rx, &mut done);
    let snapshot_b = done;
    assert_eq!(
        snapshot_a, snapshot_b,
        "segments kept completing while paused (expected no progress)"
    );
    assert!(
        snapshot_b < SEGMENTS,
        "the whole run finished before pause even took effect — test didn't \
         exercise anything"
    );

    let connections_while_paused = accepted.load(Ordering::SeqCst);

    pause.store(false, Ordering::Relaxed);

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), run)
        .await
        .expect("run did not complete after resuming")
        .unwrap()
        .unwrap();

    assert!(
        outcome.failures.is_empty(),
        "unexpected failures: {:?}",
        outcome.failures
    );
    assert_eq!(outcome.segments.len(), SEGMENTS);

    // Resuming must not have opened any new connection.
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        connections_while_paused,
        "resuming reconnected instead of reusing the paused connections"
    );
    assert!(accepted.load(Ordering::SeqCst) <= CONNECTIONS);
}

/// A mock server that stalls 50ms before answering each `POST`, so a run
/// with enough segments cannot finish inside this test's control windows by
/// accident — the pause/cancel race below needs a run that is genuinely
/// still in flight, not one that quietly completed first.
fn spawn_slow_counting_server(accepted: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            accepted.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || handle_connection_slow(stream));
        }
    });

    addr
}

fn handle_connection_slow(stream: TcpStream) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    if writer.write_all(b"200 pesto mock ready\r\n").is_err() {
        return;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end().to_string();

        if command == "POST" {
            if writer.write_all(b"340 send article\r\n").is_err() {
                return;
            }
            let mut raw = Vec::new();
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if command.starts_with("MODE READER") {
            if writer.write_all(b"200 reader mode\r\n").is_err() {
                return;
            }
        } else if command == "QUIT" {
            let _ = writer.write_all(b"205 bye\r\n");
            return;
        } else if writer.write_all(b"500 unknown command\r\n").is_err() {
            return;
        }
    }
}

/// If cancellation lands while a worker is paused, it must be noticed (and
/// the run wound down) within roughly the same latency as an unpaused
/// cancellation — not gated behind the multi-second keepalive poll interval
/// meant for idle-but-unpaused connections. The producer itself is never at
/// risk of hanging regardless of this poll granularity: `post_files_inner`
/// only ever keeps a *block-scoped* clone of the workers' shared receiver
/// (dropped once every worker task has spawned), so once the last worker
/// exits, the receiver's `Arc` strong count hits zero and any send the
/// producer is blocked on resolves with an error on its own — but a slow
/// poll here would still make every cancel-while-paused feel sluggish.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_while_paused_is_noticed_promptly_not_after_the_idle_keepalive_poll() {
    const CONNECTIONS: usize = 1;
    // At 50ms/article and one worker, finishing all of these would take a
    // full second — comfortably longer than this test's control windows, so
    // the run is guaranteed to still be in flight (and, once paused, stuck)
    // when cancellation fires.
    const SEGMENTS: usize = 20;

    let accepted = Arc::new(AtomicUsize::new(0));
    let addr = spawn_slow_counting_server(accepted);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, content(2, ARTICLE_SIZE * SEGMENTS)).unwrap();

    let config = test_config(addr.port(), CONNECTIONS);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();

    let pause = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let run = {
        let pause = pause.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            post_files_inner(
                &config,
                &inputs,
                Some(tx),
                None,
                Some(cancel),
                None,
                None,
                Some(pause),
            )
            .await
        })
    };

    // Wait for genuine progress (proves the pipeline is alive and the
    // producer is racing ahead of the single slow worker) before pausing.
    loop {
        match rx.recv().await {
            Some(ProgressEvent::SegmentDone { .. }) => break,
            Some(_) => continue,
            None => panic!("progress channel closed before any segment completed"),
        }
    }
    pause.store(true, Ordering::Relaxed);

    // Long enough for the pause mirror (50ms poll) to take effect and for
    // the producer to fill the now-undrained channel and block on a send —
    // filling the channel needs no network round trip, so this reliably
    // happens well within the 50ms/article POST latency budget.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    cancel.store(true, Ordering::Relaxed);
    let t0 = std::time::Instant::now();

    // 10s is only a last-resort backstop against an actual hang; the real
    // assertion is the tight bound below.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("cancelling while paused with a full channel hung the run")
        .unwrap()
        .unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(1000),
        "cancelling while paused took {elapsed:?} — the pause-wait loop is \
         polling at the multi-second idle-keepalive interval instead of a \
         short one, making cancel sluggish while paused"
    );

    assert!(outcome.cancelled, "expected the run to report cancelled");
    assert!(
        outcome.segments.len() < SEGMENTS,
        "expected the run to be cut short by pause+cancel, not complete \
         normally (posted {} of {SEGMENTS})",
        outcome.segments.len()
    );
}
