//! Regression guard for the "fast repost" circuit breaker in the streaming
//! `--check` queue: once a run has accumulated enough first-time STAT
//! checks with a low miss rate, an isolated miss skips the patient
//! `STAT_RETRY_DELAY_SECS`-spaced retries and reposts immediately instead
//! of waiting through them to reach an already-foregone "confirmed missing"
//! verdict. See `should_fast_repost` in `poster::check`.
//!
//! This test posts 40 segments where exactly one (the last one accepted by
//! the mock server) reports "not found" on its very first `STAT` — a
//! classic isolated miss against an otherwise clean run — and asserts:
//! the run completes quickly (nowhere near the ~20s+ a single patient retry
//! would need), and the server received exactly one repost (41 total
//! `POST`s for 40 segments), proving the miss was reposted immediately
//! rather than retried.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files_with_progress;
use pesto::walk::expand_inputs;

const SEGMENTS: usize = 40;

/// A mock NNTP server that answers every `STAT` truthfully from the set of
/// Message-IDs it has actually received via `POST` — except the *first*
/// time it's asked about the 40th distinct article it accepts, which it
/// reports missing exactly once (simulating a single article that
/// genuinely never made it, indistinguishable at check time from a slow
/// one). Any later `STAT` for that ID, or the fresh ID pesto reposts it
/// under, is answered truthfully.
struct MockState {
    posted_ids: Vec<String>,
    victim_id: Option<String>,
    victim_stat_used: bool,
}

fn spawn_mock_server() -> (SocketAddr, Arc<AtomicUsize>, Arc<Mutex<MockState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let post_count = Arc::new(AtomicUsize::new(0));
    let state = Arc::new(Mutex::new(MockState {
        posted_ids: Vec::new(),
        victim_id: None,
        victim_stat_used: false,
    }));

    let post_count_clone = Arc::clone(&post_count);
    let state_clone = Arc::clone(&state);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let post_count = Arc::clone(&post_count_clone);
            let state = Arc::clone(&state_clone);
            std::thread::spawn(move || handle_connection(stream, post_count, state));
        }
    });

    (addr, post_count, state)
}

fn handle_connection(
    stream: TcpStream,
    post_count: Arc<AtomicUsize>,
    state: Arc<Mutex<MockState>>,
) {
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
            let mut message_id = None;
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
                if let Ok(text) = std::str::from_utf8(&raw) {
                    if let Some(id) = text.strip_prefix("Message-ID: ") {
                        message_id = Some(id.trim_end().to_string());
                    }
                }
            }
            post_count.fetch_add(1, Ordering::SeqCst);
            if let Some(id) = message_id {
                let mut st = state.lock().unwrap();
                st.posted_ids.push(id);
                // The 40th distinct article this server accepts is the
                // designated victim -- reposts (the 41st+ POST) don't
                // retrigger this, since it only fires once, on the first
                // batch reaching exactly 40.
                if st.posted_ids.len() == SEGMENTS && st.victim_id.is_none() {
                    st.victim_id = st.posted_ids.last().cloned();
                }
            }
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if let Some(id) = command.strip_prefix("STAT ") {
            let id = id.trim();
            let mut st = state.lock().unwrap();
            let is_victim_first_look = st.victim_id.as_deref() == Some(id) && !st.victim_stat_used;
            if is_victim_first_look {
                st.victim_stat_used = true;
                drop(st);
                if writer.write_all(b"430 no such article found\r\n").is_err() {
                    return;
                }
            } else {
                let found = st.posted_ids.iter().any(|p| p == id);
                drop(st);
                let resp: &[u8] = if found {
                    b"223 0 <id> article exists\r\n"
                } else {
                    b"430 no such article found\r\n"
                };
                if writer.write_all(resp).is_err() {
                    return;
                }
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

#[tokio::test(flavor = "multi_thread")]
async fn check_fast_reposts_an_isolated_miss_instead_of_waiting_out_patient_retries() {
    let (addr, post_count, _state) = spawn_mock_server();

    let dir = std::env::temp_dir().join(format!("pesto_check_fast_repost_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");
    std::fs::write(&input, vec![7u8; SEGMENTS * 1000]).unwrap();

    let config = Config {
        host: addr.ip().to_string(),
        port: addr.port(),
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 1000,
        line_length: 128,
        retries: 1,
        retry_delay: 0,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
        par2_only: false,
        threads: 0,
        simd: pesto::par2::SimdPath::Auto,
        extra_servers: vec![],
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: vec![],
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
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
        message_id_domain: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: false,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: true,
        check_delay_secs: 0,
        // 3 patient retries at 20s apart -- if the fast-repost circuit
        // breaker didn't trigger, this run would take 40s+ just for the
        // victim's retries, which the elapsed-time assertion below rules
        // out.
        check_retries: 3,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: true,
        pipeline_depth: 1,
        keepalive_interval: 0,
    };

    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let start = Instant::now();
    let outcome = post_files_with_progress(&config, &inputs, None, None, None)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(
        outcome.segments.len(),
        SEGMENTS,
        "expected {SEGMENTS} segments"
    );
    assert!(
        outcome.still_missing.is_empty(),
        "the reposted copy should have been confirmed: {:?}",
        outcome.still_missing
    );
    assert_eq!(
        post_count.load(Ordering::SeqCst),
        SEGMENTS + 1,
        "expected exactly one repost (SEGMENTS original POSTs + 1 for the \
         victim) -- a different count means either no repost happened or \
         more than one did"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "run took {elapsed:?} -- the fast-repost circuit breaker should \
         have skipped the ~20s+ patient retry wait for the isolated miss \
         entirely instead of waiting it out"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
