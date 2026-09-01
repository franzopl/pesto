//! Resume confirmation flags, the three `prepare_ready` arms, and cancel
//! persistence (design T12 / T13 / T13a / T13b / T13c / T15 / T23).
//!
//! Mock NNTP only — never a real provider.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::{post_files_with_progress, post_files_with_progress_and_cancel};
use pesto::progress::ProgressEvent;
use pesto::resume::{ResumeState, RunFingerprint};

#[derive(Clone, Default)]
struct MockStats {
    posts: Arc<AtomicUsize>,
    stats: Arc<AtomicUsize>,
}

/// Accepts POST/STAT with optional: fail POSTs after `fail_after` successes,
/// reject POSTs whose Subject contains `reject_subject`, and answer STAT 430
/// for the first `cursed` distinct Message-IDs (in first-seen order).
async fn handle_connection(
    stream: TcpStream,
    counts: MockStats,
    fail_after: Option<usize>,
    reject_subject: Option<&'static str>,
    cursed: usize,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    write_half
        .write_all(b"200 pesto mock ready\r\n")
        .await
        .unwrap();

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap() == 0 {
            return;
        }
        let command = line.trim_end();

        if command.starts_with("AUTHINFO USER") {
            write_half
                .write_all(b"381 password required\r\n")
                .await
                .unwrap();
        } else if command.starts_with("AUTHINFO PASS") {
            write_half
                .write_all(b"281 authenticated\r\n")
                .await
                .unwrap();
        } else if command == "POST" {
            write_half.write_all(b"340 send article\r\n").await.unwrap();
            let mut article = Vec::new();
            let mut body = Vec::new();
            loop {
                body.clear();
                if reader.read_until(b'\n', &mut body).await.unwrap() == 0 {
                    return;
                }
                if body == b".\r\n" {
                    break;
                }
                article.extend_from_slice(&body);
            }
            let text = String::from_utf8_lossy(&article);
            let subject = text
                .lines()
                .find_map(|l| l.strip_prefix("Subject: "))
                .unwrap_or("");
            let id = text
                .lines()
                .find_map(|l| l.strip_prefix("Message-ID: "))
                .map(str::trim)
                .map(str::to_string);
            if let Some(needle) = reject_subject {
                if subject.contains(needle) {
                    write_half
                        .write_all(b"441 posting failed\r\n")
                        .await
                        .unwrap();
                    continue;
                }
            }
            let n = counts.posts.fetch_add(1, Ordering::Relaxed);
            if fail_after.is_some_and(|after| n >= after) {
                write_half
                    .write_all(b"441 posting failed\r\n")
                    .await
                    .unwrap();
                continue;
            }
            if let Some(id) = id {
                let mut seen = seen.lock().unwrap();
                if !seen.contains(&id) {
                    seen.push(id);
                }
            }
            write_half
                .write_all(b"240 article received\r\n")
                .await
                .unwrap();
        } else if let Some(id) = command.strip_prefix("STAT ") {
            counts.stats.fetch_add(1, Ordering::Relaxed);
            let found = {
                let seen = seen.lock().unwrap();
                match seen.iter().position(|x| x == id) {
                    Some(ord) => ord >= cursed,
                    // Stored resume ids were never POSTed this run — treat as
                    // present so arm 2 can confirm without a second POST.
                    None => cursed == 0,
                }
            };
            let resp = if found {
                format!("223 0 {id} article exists\r\n")
            } else {
                "430 No such article\r\n".to_string()
            };
            write_half.write_all(resp.as_bytes()).await.unwrap();
        } else if command.starts_with("MODE READER") {
            write_half.write_all(b"200 reader mode\r\n").await.unwrap();
        } else if command == "QUIT" {
            write_half.write_all(b"205 bye\r\n").await.unwrap();
            return;
        } else {
            write_half
                .write_all(b"500 unknown command\r\n")
                .await
                .unwrap();
        }
    }
}

async fn spawn_mock(
    fail_after: Option<usize>,
    reject_subject: Option<&'static str>,
    cursed: usize,
) -> (u16, MockStats) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counts = MockStats::default();
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let counts = counts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(
                    stream,
                    counts.clone(),
                    fail_after,
                    reject_subject,
                    cursed,
                    seen.clone(),
                ));
            }
        });
    }
    (addr.port(), counts)
}

fn test_config(port: u16, check: bool) -> Config {
    Config {
        host: "127.0.0.1".to_string(),
        port,
        ssl: false,
        connections: 2,
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 100,
        line_length: 128,
        retries: 1,
        retry_delay: 0,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        proxy: None,
        proxy_check_ip: false,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_memory_limit: Some(1_000_000_000),
        memory_limit: None,
        par2_temp_dir: None,
        compress_temp_dir: None,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_only: false,
        par2_before_upload: false,
        threads: 0,
        simd: parmesan::SimdPath::Auto,
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
        check,
        check_delay_secs: 0,
        check_retries: 1,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        check_recover_percent: 15,
        check_recover_max: 0,
        pipeline_depth: 1,
        keepalive_interval: 0,
    }
}

fn input(dir: &std::path::Path, name: &str, bytes: usize) -> pesto::walk::InputFile {
    let path = dir.join(name);
    std::fs::write(&path, vec![0xABu8; bytes]).unwrap();
    pesto::walk::InputFile {
        path,
        name: name.to_string(),
    }
}

/// Write a `.pesto-state` whose segment objects are `{message_id, bytes}`
/// only — the on-disk shape before `confirmed`/`check_disabled`/`server_idx`.
fn write_pre_schema_state(
    path: &std::path::Path,
    config: &Config,
    file_name: &str,
    segs: &[(&str, u64)],
) {
    let mut state = ResumeState::default();
    state.validate_run(&RunFingerprint::from_config(config));
    for (i, (id, bytes)) in segs.iter().enumerate() {
        state.record(file_name, (i as u32) + 1, id, *bytes);
    }
    state.save(path).unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    if let Some(segments) = v.get_mut("segments").and_then(|s| s.as_object_mut()) {
        for rec in segments.values_mut() {
            if let Some(obj) = rec.as_object_mut() {
                obj.remove("confirmed");
                obj.remove("check_disabled");
                obj.remove("server_idx");
            }
        }
    }
    std::fs::write(path, serde_json::to_string(&v).unwrap()).unwrap();
}

/// T12: `--no-check` incomplete (POST fail) persists `(confirmed=false,
/// check_disabled=true)` and never `confirmed=true`.
#[tokio::test(flavor = "multi_thread")]
async fn t12_no_check_incomplete_persists_check_disabled() {
    let (port, counts) = spawn_mock(Some(1), None, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "partial.bin", 150); // 2 segments
    let state_path = dir.path().join("partial.bin.pesto-state");
    let mut config = test_config(port, false);
    config.connections = 1;

    let outcome = post_files_with_progress(&config, &[file], None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(
        !outcome.failed_tasks.is_empty(),
        "second segment should fail POST"
    );
    assert!(
        state_path.exists(),
        "incomplete --no-check must persist state"
    );
    let state = ResumeState::load(&state_path).unwrap();
    assert!(
        !state.is_empty(),
        "at least one 240 must be recorded on an incomplete --no-check run"
    );
    for part in [1u32, 2] {
        if let Some(rec) = state.get("partial.bin", part) {
            assert!(!rec.confirmed, "--no-check must never write confirmed=true");
            assert!(rec.check_disabled);
        }
    }
    assert!(counts.posts.load(Ordering::Relaxed) >= 1);
}

/// T13: new-schema fixture + `--resume --check` skips `confirmed`, re-STATs
/// unconfirmed (0 extra POSTs if 223), POSTs missing.
#[tokio::test(flavor = "multi_thread")]
async fn t13_new_schema_three_arms() {
    let (port, counts) = spawn_mock(None, None, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 250); // 3 segments
    let state_path = dir.path().join("movie.bin.pesto-state");
    let mut config = test_config(port, true);
    config.resume = true;

    let mut prior = ResumeState::default();
    prior.validate_run(&RunFingerprint::from_config(&config));
    prior.record_with(
        "movie.bin",
        1,
        pesto::resume::SegmentRecord {
            message_id: "conf@pre.example".into(),
            bytes: 100,
            confirmed: true,
            check_disabled: false,
            server_idx: 0,
            wire_identity: None,
        },
    );
    prior.record_with(
        "movie.bin",
        2,
        pesto::resume::SegmentRecord {
            message_id: "open@pre.example".into(),
            bytes: 100,
            confirmed: false,
            check_disabled: false,
            server_idx: 0,
            wire_identity: None,
        },
    );
    prior.save(&state_path).unwrap();

    let outcome = post_files_with_progress(&config, &[file], None, Some(&state_path), None)
        .await
        .unwrap();

    let ids: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.message_id.as_str())
        .collect();
    assert!(
        ids.contains(&"conf@pre.example"),
        "arm 1 reuses confirmed id"
    );
    assert!(
        ids.contains(&"open@pre.example"),
        "arm 2 reuses unconfirmed id"
    );
    assert_eq!(
        counts.posts.load(Ordering::Relaxed),
        1,
        "only the missing part should POST"
    );
    assert!(
        counts.stats.load(Ordering::Relaxed) >= 1,
        "arm 2 must STAT the stored unconfirmed id"
    );
}

/// T13a: pre-schema `{message_id, bytes}` + `--resume --no-check` → skip, 0 POSTs.
#[tokio::test(flavor = "multi_thread")]
async fn t13a_pre_schema_no_check_skips() {
    let (port, counts) = spawn_mock(None, None, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 250);
    let state_path = dir.path().join("movie.bin.pesto-state");
    let mut config = test_config(port, false);
    config.resume = true;
    write_pre_schema_state(
        &state_path,
        &config,
        "movie.bin",
        &[
            ("seg1@pre.example", 100),
            ("seg2@pre.example", 100),
            ("seg3@pre.example", 50),
        ],
    );

    let outcome = post_files_with_progress(&config, &[file], None, Some(&state_path), None)
        .await
        .unwrap();

    assert_eq!(outcome.segments.len(), 3);
    assert_eq!(counts.posts.load(Ordering::Relaxed), 0);
    let ids: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.message_id.as_str())
        .collect();
    assert!(ids.contains(&"seg1@pre.example"));
    assert!(ids.contains(&"seg2@pre.example"));
    assert!(ids.contains(&"seg3@pre.example"));
}

/// T13b: same pre-schema fixture + `--resume --check` → re-STAT stored ids,
/// 0 extra POSTs if STAT 223.
#[tokio::test(flavor = "multi_thread")]
async fn t13b_pre_schema_check_restats_without_post() {
    let (port, counts) = spawn_mock(None, None, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 250);
    let state_path = dir.path().join("movie.bin.pesto-state");
    let mut config = test_config(port, true);
    config.resume = true;
    write_pre_schema_state(
        &state_path,
        &config,
        "movie.bin",
        &[
            ("seg1@pre.example", 100),
            ("seg2@pre.example", 100),
            ("seg3@pre.example", 50),
        ],
    );

    let outcome = post_files_with_progress(&config, &[file], None, Some(&state_path), None)
        .await
        .unwrap();

    assert_eq!(outcome.segments.len(), 3);
    assert_eq!(
        counts.posts.load(Ordering::Relaxed),
        0,
        "arm 2 must not POST stored ids"
    );
    assert!(
        counts.stats.load(Ordering::Relaxed) >= 3,
        "arm 2 must STAT each stored id"
    );
    let ids: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.message_id.as_str())
        .collect();
    assert!(ids.contains(&"seg1@pre.example"));
}

/// T13c: T13b wrapped in a timeout — the run must return (no hang in
/// `finish_and_drain` from a leftover `check_tx` sender on Shared).
#[tokio::test(flavor = "multi_thread")]
async fn t13c_restat_run_returns_without_hanging() {
    let (port, _counts) = spawn_mock(None, None, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 250);
    let state_path = dir.path().join("movie.bin.pesto-state");
    let mut config = test_config(port, true);
    config.resume = true;
    write_pre_schema_state(
        &state_path,
        &config,
        "movie.bin",
        &[
            ("seg1@pre.example", 100),
            ("seg2@pre.example", 100),
            ("seg3@pre.example", 50),
        ],
    );

    let files = [file];
    let run = post_files_with_progress(&config, &files, None, Some(&state_path), None);
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("resume --check arm 2 hung in finish_and_drain (check_tx not taken)")
        .unwrap();
}

/// T15: a successful check-repost overwrites resume (and the NZB) with the
/// new Message-ID, not the cursed original.
#[tokio::test(flavor = "multi_thread")]
async fn t15_repost_persists_new_message_id() {
    let (port, _counts) = spawn_mock(None, Some("fail.bin"), 1).await;
    let dir = tempfile::tempdir().unwrap();
    let ok = input(dir.path(), "ok.bin", 80);
    let fail = input(dir.path(), "fail.bin", 80);
    let state_path = dir.path().join("release.pesto-state");
    let mut config = test_config(port, true);
    config.connections = 2;
    config.check_connections = 1;
    config.check_retries = 1;
    config.check_post_retries = 1;
    config.check_recover_max = 0;

    let outcome = post_files_with_progress(&config, &[ok, fail], None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(
        !outcome.failed_tasks.is_empty(),
        "fail.bin must keep the run incomplete so state is saved"
    );
    let ok_seg = outcome
        .segments
        .iter()
        .find(|s| s.file_name == "ok.bin")
        .expect("ok.bin must appear in results");
    assert!(
        state_path.exists(),
        "incomplete run must persist resume state"
    );
    let state = ResumeState::load(&state_path).unwrap();
    let rec = state
        .get("ok.bin", 1)
        .expect("reposted segment must be recorded");
    assert_eq!(
        rec.message_id, ok_seg.message_id,
        "resume must store the new (not cursed) Message-ID"
    );

    // `--resume --check` must reuse that new id, not POST a third copy of ok.bin.
    let (port2, counts2) = spawn_mock(None, Some("fail.bin"), 0).await;
    let mut config2 = test_config(port2, true);
    config2.resume = true;
    config2.check_recover_max = 0;
    let ok2 = pesto::walk::InputFile {
        path: dir.path().join("ok.bin"),
        name: "ok.bin".into(),
    };
    let fail2 = pesto::walk::InputFile {
        path: dir.path().join("fail.bin"),
        name: "fail.bin".into(),
    };
    let outcome2 = post_files_with_progress(&config2, &[ok2, fail2], None, Some(&state_path), None)
        .await
        .unwrap();
    let ok_seg2 = outcome2
        .segments
        .iter()
        .find(|s| s.file_name == "ok.bin")
        .unwrap();
    assert_eq!(ok_seg2.message_id, rec.message_id);
    assert_eq!(
        counts2.posts.load(Ordering::Relaxed),
        0,
        "ok.bin must not be POSTed again; rejected fail.bin POSTs are not counted"
    );
}

/// T23: cancel during check drain persists state and does not strip ids.
#[tokio::test(flavor = "multi_thread")]
async fn t23_cancel_during_check_drain_persists_unconfirmed() {
    let (port, _counts) = spawn_mock(None, None, 0).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 150); // 2 segments
    let state_path = dir.path().join("movie.bin.pesto-state");
    let mut config = test_config(port, true);
    config.check_delay_secs = 30;
    config.check_recover_max = 0;

    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let run = {
        let cancel = cancel.clone();
        let file = file.clone();
        let state_path = state_path.clone();
        tokio::spawn(async move {
            post_files_with_progress_and_cancel(
                &config,
                &[file],
                Some(tx),
                Some(&state_path),
                Some(cancel),
                None,
            )
            .await
        })
    };

    let mut done = 0usize;
    while let Some(ev) = rx.recv().await {
        if let ProgressEvent::SegmentDone { ok: true, .. } = ev {
            done += 1;
            if done >= 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        }
    }
    let outcome = run.await.unwrap().unwrap();
    assert!(outcome.cancelled, "cancel flag must abort the run");
    assert!(
        state_path.exists(),
        "cancel with Posted records must persist .pesto-state"
    );
    let state = ResumeState::load(&state_path).unwrap();
    assert!(
        state.get("movie.bin", 1).is_some() && state.get("movie.bin", 2).is_some(),
        "cancel must not strip still_missing ids"
    );
    for part in [1u32, 2] {
        let rec = state.get("movie.bin", part).unwrap();
        assert!(
            !rec.confirmed,
            "cancel during drain must persist confirmed=false (STAT never finished)"
        );
    }

    // `--resume --check` re-STATs those ids, 0 extra POSTs.
    let (port2, counts2) = spawn_mock(None, None, 0).await;
    let mut config2 = test_config(port2, true);
    config2.resume = true;
    let file2 = pesto::walk::InputFile {
        path: dir.path().join("movie.bin"),
        name: "movie.bin".into(),
    };
    let outcome2 = post_files_with_progress(&config2, &[file2], None, Some(&state_path), None)
        .await
        .unwrap();
    assert_eq!(outcome2.segments.len(), 2);
    assert_eq!(counts2.posts.load(Ordering::Relaxed), 0);
    assert!(counts2.stats.load(Ordering::Relaxed) >= 2);
}
