//! End-to-end regression guard for the automatic final recovery pass added
//! alongside issue #18's resume follow-up (`check_recover_percent` /
//! `check_recover_max`, see `poster::is_cheap_to_recover` /
//! `poster::check::recover_missing`).
//!
//! This is the primary real-world case the feature targets: posting
//! finishes, the streaming check can't confirm an article even after every
//! `check_post_retries` round, and — because so few articles are missing —
//! `pesto` makes one more repost-and-verify attempt automatically, in the
//! same run, instead of requiring the caller to notice the failure and
//! rerun with `--resume`.
//!
//! Every other integration test in this crate sets `check_recover_max: 0`
//! (disabling the pass) to avoid an unrelated side effect — meaning this
//! file is the only place the feature actually runs end to end.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files_with_progress;
use pesto::walk::expand_inputs;

/// A mock NNTP server for a single-segment upload: every `POST` succeeds,
/// but the first two `STAT` commands it receives report the article
/// missing — modelling the normal check-and-repost cycle (one STAT on the
/// original copy, one on the single `check_post_retries` repost) both
/// failing — while the third and any later `STAT` succeed, modelling the
/// automatic recovery pass's own repost finally landing.
fn spawn_mock_server() -> (SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let post_count = Arc::new(AtomicUsize::new(0));
    let stat_count = Arc::new(AtomicUsize::new(0));

    let post_count_clone = Arc::clone(&post_count);
    let stat_count_clone = Arc::clone(&stat_count);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let post_count = Arc::clone(&post_count_clone);
            let stat_count = Arc::clone(&stat_count_clone);
            std::thread::spawn(move || handle_connection(stream, post_count, stat_count));
        }
    });

    (addr, post_count, stat_count)
}

fn handle_connection(
    stream: TcpStream,
    post_count: Arc<AtomicUsize>,
    stat_count: Arc<AtomicUsize>,
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
            post_count.fetch_add(1, Ordering::SeqCst);
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if command.starts_with("STAT ") {
            let seen = stat_count.fetch_add(1, Ordering::SeqCst);
            let resp: &[u8] = if seen < 2 {
                b"430 no such article found\r\n"
            } else {
                b"223 0 <id> article exists\r\n"
            };
            if writer.write_all(resp).is_err() {
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

fn test_config(addr: SocketAddr) -> Config {
    Config {
        host: addr.ip().to_string(),
        port: addr.port(),
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 100_000,
        line_length: 128,
        retries: 1,
        retry_delay: 0,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        proxy: None,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
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
        check: true,
        check_delay_secs: 0,
        // A single STAT attempt per copy and a single check_post_retries
        // round: the normal cycle gets exactly 2 chances (original +
        // 1 repost) before giving up, both of which the mock server fails.
        check_retries: 1,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        check_recover_percent: 15,
        check_recover_max: 5,
        pipeline_depth: 1,
        keepalive_interval: 0,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_recovery_pass_resolves_a_stubborn_miss_after_normal_retries_are_exhausted() {
    let (addr, post_count, stat_count) = spawn_mock_server();

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![7u8; 50_000]).unwrap();

    let config = test_config(addr);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files_with_progress(&config, &inputs, None, None, None)
        .await
        .unwrap();

    assert!(
        outcome.still_missing.is_empty(),
        "the automatic recovery pass should have resolved the stubborn miss: {:?}",
        outcome.still_missing
    );
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), 1);
    // 1 original POST + 1 check_post_retries repost + 1 recovery-pass repost.
    assert_eq!(
        post_count.load(Ordering::SeqCst),
        3,
        "expected the original POST plus exactly two reposts"
    );
    // 2 failing STATs (original + first repost) + 1 succeeding STAT (recovery repost).
    assert_eq!(stat_count.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_pass_disabled_leaves_the_segment_missing() {
    let (addr, post_count, stat_count) = spawn_mock_server();

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![7u8; 50_000]).unwrap();

    let mut config = test_config(addr);
    config.check_recover_max = 0; // matches every other test file's default

    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files_with_progress(&config, &inputs, None, None, None)
        .await
        .unwrap();

    assert_eq!(
        outcome.still_missing.len(),
        1,
        "with recovery disabled the stubborn miss must remain unresolved"
    );
    // Only the normal cycle ran: 1 original POST + 1 check_post_retries repost.
    assert_eq!(post_count.load(Ordering::SeqCst), 2);
    assert_eq!(stat_count.load(Ordering::SeqCst), 2);
}

/// A mock NNTP server for the multi-article recovery test below: every
/// `POST` succeeds, and each *part's* first two `STAT` checks (across its
/// original copy and its one `check_post_retries` repost) report the
/// article missing — every later `STAT` for that same part succeeds,
/// modelling the automatic recovery pass's own repost finally landing.
///
/// Unlike [`spawn_mock_server`]'s single global `stat_count`, failures here
/// are tracked *per part* (keyed off the POSTed article's `Subject` header,
/// which `poster::check::repost_one` always reconstructs identically across
/// every repost of the same segment — see its `default_subject` call).
/// A global counter can't express "this part's first two STATs fail" once
/// several parts' repost-and-verify cycles are running concurrently on
/// separate connections (the whole point of parallelising
/// `recover_missing`): STATs for different parts would interleave and blow
/// through a shared counter in the wrong order.
fn spawn_mock_server_multi_article() -> (SocketAddr, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let post_count = Arc::new(AtomicUsize::new(0));
    let stat_count = Arc::new(AtomicUsize::new(0));
    let subject_by_id: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let stat_attempts_by_subject: Arc<Mutex<HashMap<String, u32>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let post_count_clone = Arc::clone(&post_count);
    let stat_count_clone = Arc::clone(&stat_count);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let post_count = Arc::clone(&post_count_clone);
            let stat_count = Arc::clone(&stat_count_clone);
            let subject_by_id = Arc::clone(&subject_by_id);
            let stat_attempts_by_subject = Arc::clone(&stat_attempts_by_subject);
            std::thread::spawn(move || {
                handle_connection_multi(
                    stream,
                    post_count,
                    stat_count,
                    subject_by_id,
                    stat_attempts_by_subject,
                )
            });
        }
    });

    (addr, post_count, stat_count)
}

fn handle_connection_multi(
    stream: TcpStream,
    post_count: Arc<AtomicUsize>,
    stat_count: Arc<AtomicUsize>,
    subject_by_id: Arc<Mutex<HashMap<String, String>>>,
    stat_attempts_by_subject: Arc<Mutex<HashMap<String, u32>>>,
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
            let mut message_id = String::new();
            let mut subject = String::new();
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
                if let Ok(text) = std::str::from_utf8(&raw) {
                    if let Some(v) = text.strip_prefix("Message-ID: ") {
                        message_id = v.trim_end().to_string();
                    } else if let Some(v) = text.strip_prefix("Subject: ") {
                        subject = v.trim_end().to_string();
                    }
                }
            }
            if !message_id.is_empty() && !subject.is_empty() {
                subject_by_id.lock().unwrap().insert(message_id, subject);
            }
            post_count.fetch_add(1, Ordering::SeqCst);
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if let Some(id) = command.strip_prefix("STAT ") {
            stat_count.fetch_add(1, Ordering::SeqCst);
            let subject = subject_by_id.lock().unwrap().get(id).cloned();
            let attempt = subject.map(|s| {
                let mut attempts = stat_attempts_by_subject.lock().unwrap();
                let n = attempts.entry(s).or_insert(0);
                *n += 1;
                *n
            });
            // First two STAT attempts for a given part (original + one
            // check_post_retries repost) report missing; the third (the
            // recovery pass's own repost) is confirmed present.
            let resp: &[u8] = if attempt.is_none_or(|n| n <= 2) {
                b"430 no such article found\r\n"
            } else {
                b"223 0 <id> article exists\r\n"
            };
            if writer.write_all(resp).is_err() {
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

#[tokio::test(flavor = "multi_thread")]
async fn auto_recovery_pass_resolves_several_concurrent_stubborn_misses() {
    // Regression guard for parallelising `recover_missing`: before it, a
    // batch this size was reposted-and-verified strictly one article at a
    // time (repost + `check_delay_secs` + STAT, serially), so a handful of
    // stubborn misses could take minutes with the upload already sitting at
    // 100%. `check_connections: 3` gives the recovery pass room to actually
    // run several of these concurrently instead of only ever exercising the
    // single-worker path the smaller test above already covers.
    let (addr, post_count, stat_count) = spawn_mock_server_multi_article();

    let dir = tempfile::tempdir().unwrap();
    const N: usize = 6;
    let inputs: Vec<_> = (0..N)
        .map(|i| {
            let path = dir.path().join(format!("movie{i}.bin"));
            std::fs::write(&path, vec![(i + 1) as u8; 5_000]).unwrap();
            path
        })
        .collect();

    let mut config = test_config(addr);
    config.check_connections = 3;
    config.check_recover_percent = 100;
    config.check_recover_max = N;

    let expanded = expand_inputs(&inputs).unwrap();
    let outcome = post_files_with_progress(&config, &expanded, None, None, None)
        .await
        .unwrap();

    assert!(
        outcome.still_missing.is_empty(),
        "the automatic recovery pass should have resolved every stubborn miss: {:?}",
        outcome.still_missing
    );
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), N);
    // Per part: 1 original POST + 1 check_post_retries repost + 1 recovery-pass repost.
    assert_eq!(post_count.load(Ordering::SeqCst), N * 3);
    // Per part: 2 failing STATs (original + first repost) + 1 succeeding STAT.
    assert_eq!(stat_count.load(Ordering::SeqCst), N * 3);
}
