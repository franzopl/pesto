//! End-to-end test: post a file through `post_files` against an in-process
//! mock NNTP server speaking just enough of the protocol.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::{post_files, post_files_with_progress};

/// Handle one mock NNTP connection: greet, accept auth, ack each `POST`.
async fn handle_connection(stream: TcpStream, posts: Arc<AtomicUsize>) {
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
            // Consume the article up to the terminating ".\r\n". The yEnc body
            // is not valid UTF-8, so it must be read as raw bytes.
            let mut body = Vec::new();
            loop {
                body.clear();
                if reader.read_until(b'\n', &mut body).await.unwrap() == 0 {
                    return;
                }
                if body == b".\r\n" {
                    break;
                }
            }
            posts.fetch_add(1, Ordering::Relaxed);
            write_half
                .write_all(b"240 article received\r\n")
                .await
                .unwrap();
        } else if command == "QUIT" {
            write_half.write_all(b"205 bye\r\n").await.unwrap();
            return;
        }
    }
}

#[tokio::test]
async fn posts_every_segment_to_a_mock_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));

    {
        let posts = posts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(stream, posts.clone()));
            }
        });
    }

    // A 250-byte file with a 100-byte article size yields three segments.
    let path = std::env::temp_dir().join(format!("pesto_it_{}.bin", std::process::id()));
    std::fs::write(&path, vec![0xABu8; 250]).unwrap();

    let config = Config {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        ssl: false,
        connections: 2,
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 100,
        line_length: 128,
        retries: 3,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_memory_limit: Some(1_000_000_000),
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
        history: true,
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
        check_delay_secs: 30,
        check_retries: 2,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        check_recover_percent: 15,
        check_recover_max: 0,
        pipeline_depth: 1,
        keepalive_interval: 0,
    };

    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "pesto_it.bin".to_string(),
    }];
    let outcome = post_files(&config, &inputs).await.unwrap();
    std::fs::remove_file(&path).ok();

    assert!(
        outcome.failures.is_empty(),
        "unexpected failures: {:?}",
        outcome.failures
    );
    assert!(!outcome.cancelled);
    assert_eq!(outcome.segments.len(), 3);
    assert_eq!(posts.load(Ordering::Relaxed), 3);

    // The collected segments must be enough to build a valid .nzb.
    let nzb = pesto::nzb::generate(
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
    );
    assert_eq!(nzb.matches("<segment ").count(), 3);
    assert!(nzb.contains("<file "));
}

/// Mock server that rejects the first `fail_count` POST commands with 441,
/// then accepts all subsequent ones normally.
async fn handle_connection_with_failures(
    stream: TcpStream,
    posts: Arc<AtomicUsize>,
    fail_count: Arc<AtomicUsize>,
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
            // Decide before reading body whether to fail or succeed.
            let remaining = fail_count.load(Ordering::Relaxed);
            if remaining > 0 {
                // Reject without a send-article prompt so the client sees a
                // non-240 response and marks the slot invalid, triggering retry.
                write_half
                    .write_all(b"440 posting not allowed\r\n")
                    .await
                    .unwrap();
                fail_count.fetch_sub(1, Ordering::Relaxed);
                // Close after rejection so the connection is clearly dead.
                return;
            }
            write_half.write_all(b"340 send article\r\n").await.unwrap();
            let mut body = Vec::new();
            loop {
                body.clear();
                if reader.read_until(b'\n', &mut body).await.unwrap() == 0 {
                    return;
                }
                if body == b".\r\n" {
                    break;
                }
            }
            posts.fetch_add(1, Ordering::Relaxed);
            write_half
                .write_all(b"240 article received\r\n")
                .await
                .unwrap();
        } else if command == "QUIT" {
            write_half.write_all(b"205 bye\r\n").await.unwrap();
            return;
        }
    }
}

/// Build a minimal Config pointing at the given mock server address.
fn make_config(port: u16) -> Config {
    Config {
        host: "127.0.0.1".to_string(),
        port,
        ssl: false,
        connections: 1,
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 100,
        line_length: 128,
        retries: 5,
        retry_delay: 0,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_memory_limit: Some(1_000_000_000),
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
        check_delay_secs: 30,
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

/// An article that is rejected twice (440) eventually succeeds on the third
/// attempt because the poster retries up to `config.retries` times.
#[tokio::test]
async fn retry_succeeds_after_transient_failures() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    // Fail the first 2 POST commands; the 3rd should succeed.
    let fail_count = Arc::new(AtomicUsize::new(2));

    {
        let posts = posts.clone();
        let fail_count = fail_count.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection_with_failures(
                    stream,
                    posts.clone(),
                    fail_count.clone(),
                ));
            }
        });
    }

    // Single-segment file (80 bytes < article_size 100).
    let path = std::env::temp_dir().join(format!("pesto_retry_{}.bin", std::process::id()));
    std::fs::write(&path, vec![0x42u8; 80]).unwrap();

    let config = make_config(addr.port());
    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "retry_test.bin".to_string(),
    }];
    let outcome = post_files(&config, &inputs).await.unwrap();
    std::fs::remove_file(&path).ok();

    assert!(
        outcome.failures.is_empty(),
        "expected no failures after retries, got: {:?}",
        outcome.failures
    );
    assert_eq!(outcome.segments.len(), 1);
    // Exactly one POST reached the mock server (the two failures closed
    // their connections before reading the body).
    assert_eq!(posts.load(Ordering::Relaxed), 1);
}

/// When `resume = true` and a state file already contains every segment of the
/// file, no articles are sent to the server — the stored Message-IDs are reused.
#[tokio::test]
async fn resume_skips_already_posted_segments() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));

    {
        let posts = posts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(stream, posts.clone()));
            }
        });
    }

    // 250-byte file → 3 segments with article_size=100.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resume_test.bin");
    std::fs::write(&path, vec![0xCDu8; 250]).unwrap();

    // Pre-populate the resume state with all 3 segments.
    let state_path = dir.path().join("resume_test.bin.pesto-state");
    let mut state = pesto::resume::ResumeState::default();
    state.record("resume_test.bin", 1, "seg1@preposted.example", 100);
    state.record("resume_test.bin", 2, "seg2@preposted.example", 100);
    state.record("resume_test.bin", 3, "seg3@preposted.example", 50);
    state.save(&state_path).unwrap();

    let mut config = make_config(addr.port());
    config.resume = true;

    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "resume_test.bin".to_string(),
    }];
    let outcome = post_files_with_progress(&config, &inputs, None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(
        outcome.failures.is_empty(),
        "unexpected failures: {:?}",
        outcome.failures
    );
    // All 3 segments were already in the state → NZB must reference them.
    assert_eq!(outcome.segments.len(), 3);
    // The mock server must have received zero POSTs.
    assert_eq!(
        posts.load(Ordering::Relaxed),
        0,
        "expected 0 POSTs but server saw some — resume did not skip"
    );
    // Stored Message-IDs are reused verbatim in the outcome.
    let ids: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.message_id.as_str())
        .collect();
    assert!(ids.contains(&"seg1@preposted.example"));
    assert!(ids.contains(&"seg2@preposted.example"));
    assert!(ids.contains(&"seg3@preposted.example"));
}

/// Resume state must be tracked and persisted on an incomplete run even when
/// `--resume` was never passed — otherwise a user who didn't think to opt in
/// up front (the common case: nobody expects a failure before it happens)
/// has nothing to recover from after the fact. See issue #18.
#[tokio::test]
async fn resume_state_is_saved_after_failure_even_without_the_resume_flag() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));
    // Reject every attempt, on every connection, so the segment exhausts
    // `config.retries` and lands in `failed_tasks` no matter how many times
    // it reconnects.
    let fail_count = Arc::new(AtomicUsize::new(1_000));

    {
        let posts = posts.clone();
        let fail_count = fail_count.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection_with_failures(
                    stream,
                    posts.clone(),
                    fail_count.clone(),
                ));
            }
        });
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("never_posts.bin");
    std::fs::write(&path, vec![0xAB_u8; 80]).unwrap();
    let state_path = dir.path().join("never_posts.bin.pesto-state");

    let mut config = make_config(addr.port());
    config.resume = false; // deliberately not passed
    config.retries = 2;

    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "never_posts.bin".to_string(),
    }];
    let outcome = post_files_with_progress(&config, &inputs, None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(
        !outcome.failed_tasks.is_empty(),
        "expected the segment to end up in failed_tasks after exhausting retries"
    );
    assert!(
        state_path.exists(),
        "resume state should have been written for a later retry, even though \
         --resume was not passed on this run"
    );
}

/// A run that completes fully must have no leftover resume state — both a
/// freshly-tracked one from this run, and any stale file left over from an
/// earlier failed attempt at the same output path (the exact hazard behind
/// issue #18's "silent no-op re-posts" bug: an old state file left on disk
/// after a successful run gets blindly trusted by a later invocation).
#[tokio::test]
async fn resume_state_is_deleted_after_a_fully_successful_run() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));

    {
        let posts = posts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(stream, posts.clone()));
            }
        });
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("succeeds.bin");
    std::fs::write(&path, vec![0xCD_u8; 80]).unwrap();
    let state_path = dir.path().join("succeeds.bin.pesto-state");

    // Simulate a stale leftover from an earlier, unrelated failed run at the
    // same output path — exactly what issue #18 warns must not be trusted
    // (or left behind) once this run succeeds.
    let mut stale = pesto::resume::ResumeState::default();
    stale.record("succeeds.bin", 1, "stale@leftover.example", 80);
    stale.save(&state_path).unwrap();

    let config = make_config(addr.port()); // resume: false, per make_config's default

    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "succeeds.bin".to_string(),
    }];
    let outcome = post_files_with_progress(&config, &inputs, None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), 1);
    // The stale ID must not have been reused — a real POST happened.
    assert_eq!(posts.load(Ordering::Relaxed), 1);
    assert!(
        !state_path.exists(),
        "resume state must be deleted once the run completes successfully"
    );
}

/// Reproduces issue #18 bug #3 verbatim: a `--resume` run using a different
/// `--article-size` than the run that originally populated the state must
/// not trust the stale segments (whose Message-IDs reference different,
/// wrongly-sized byte ranges) — it must detect the mismatch, discard the
/// whole state, and actually re-post under the new geometry.
#[tokio::test]
async fn resume_with_different_article_size_discards_stale_state_instead_of_corrupting_output() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));

    {
        let posts = posts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(stream, posts.clone()));
            }
        });
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resize.bin");
    std::fs::write(&path, vec![0xEF_u8; 200]).unwrap();
    let state_path = dir.path().join("resize.bin.pesto-state");

    // A prior run posted this file with article_size=100 (2 segments) and
    // its state was never cleaned up (e.g. the run was killed before the
    // final persist-or-delete decision could run). `validate_run` is called
    // here to record the fingerprint that run would have recorded, so this
    // accurately simulates a real prior run rather than a hand-crafted
    // pre-fingerprint state file.
    let mut stale = pesto::resume::ResumeState::default();
    stale.validate_run(&pesto::resume::RunFingerprint {
        article_size: 100,
        obfuscate: ObfuscateMode::None,
        compress_format: None,
        par2_percent: 0,
        file_counter: false,
    });
    stale.record("resize.bin", 1, "stale-part1@x", 100);
    stale.record("resize.bin", 2, "stale-part2@x", 100);
    stale.save(&state_path).unwrap();

    let mut config = make_config(addr.port());
    config.resume = true;
    config.article_size = 40; // different geometry: 5 segments, not 2

    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "resize.bin".to_string(),
    }];
    let outcome = post_files_with_progress(&config, &inputs, None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(outcome.failures.is_empty());
    // The new article_size produces 5 segments, not the stale 2 — proof the
    // old state was discarded rather than partially trusted.
    assert_eq!(outcome.segments.len(), 5);
    assert_eq!(
        posts.load(Ordering::Relaxed),
        5,
        "every segment must have been freshly posted, not skipped via stale state"
    );
    let ids: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.message_id.as_str())
        .collect();
    assert!(!ids.contains(&"stale-part1@x"));
    assert!(!ids.contains(&"stale-part2@x"));
}

/// `ObfuscateMode::FullShared` gives every file in a release the same wire
/// name prefix so indexers can group them — but that prefix is normally
/// randomly regenerated every run, which would otherwise make a `--resume`
/// continuation post its remaining files under a *different* prefix than the
/// segments the interrupted run already confirmed, splitting one release's
/// articles across two indexer-visible names. A compatible prior state must
/// make the continuation reuse the exact same prefix.
#[tokio::test]
async fn resume_reuses_the_full_shared_prefix_across_runs() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));

    {
        let posts = posts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(stream, posts.clone()));
            }
        });
    }

    let dir = tempfile::tempdir().unwrap();
    let a_path = dir.path().join("a.bin");
    let b_path = dir.path().join("b.bin");
    std::fs::write(&a_path, vec![0x11_u8; 50]).unwrap();
    std::fs::write(&b_path, vec![0x22_u8; 50]).unwrap();
    let state_path = dir.path().join("release.pesto-state");

    let mut config = make_config(addr.port());
    config.resume = true;
    config.obfuscate = ObfuscateMode::FullShared;

    // Simulate an interrupted prior run: `a.bin` already confirmed posted,
    // under a prefix that run generated, `b.bin` never reached.
    let a_md = std::fs::metadata(&a_path).unwrap();
    let a_mtime = a_md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let mut prior = pesto::resume::ResumeState::default();
    prior.validate_run(&pesto::resume::RunFingerprint {
        article_size: config.article_size as u64,
        obfuscate: ObfuscateMode::FullShared,
        compress_format: None,
        par2_percent: 0,
        file_counter: false,
    });
    prior.record_file(
        "a.bin",
        pesto::resume::FileFingerprint {
            size: a_md.len(),
            mtime: a_mtime,
        },
    );
    prior.record("a.bin", 1, "a-part1@prior-run.example", 60);
    prior.set_release_identity(
        "PriorPrefix123".to_string(),
        "prior@sender.example".to_string(),
    );
    prior.save(&state_path).unwrap();

    let inputs = vec![
        pesto::walk::InputFile {
            path: a_path,
            name: "a.bin".to_string(),
        },
        pesto::walk::InputFile {
            path: b_path,
            name: "b.bin".to_string(),
        },
    ];
    let outcome = post_files_with_progress(&config, &inputs, None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), 2);

    let a_seg = outcome
        .segments
        .iter()
        .find(|s| s.file_name == "a.bin")
        .unwrap();
    let b_seg = outcome
        .segments
        .iter()
        .find(|s| s.file_name == "b.bin")
        .unwrap();

    // a.bin was resumed from state, untouched.
    assert_eq!(a_seg.message_id, "a-part1@prior-run.example");
    // b.bin was freshly posted this run.
    assert_ne!(b_seg.message_id, "a-part1@prior-run.example");
    assert_eq!(
        posts.load(Ordering::Relaxed),
        1,
        "only b.bin should have actually been posted"
    );

    // Both must carry the *reused* prefix, not a freshly-generated one —
    // otherwise the release would be split across two prefixes.
    assert!(
        a_seg.subject_name.starts_with("PriorPrefix123"),
        "a.bin subject was `{}`",
        a_seg.subject_name
    );
    assert!(
        b_seg.subject_name.starts_with("PriorPrefix123"),
        "b.bin subject was `{}`",
        b_seg.subject_name
    );
}

/// Type-1 spool: a segment that was encoded and (maybe) sent before an
/// interruption, but never confirmed by the streaming check nor recorded in
/// resume state, must be replayed byte-for-byte under its original
/// Message-ID on `--resume` — not silently re-encoded and posted under a
/// fresh one, which would risk a duplicate article if the original `POST`
/// had actually landed. See `pesto::spool` / GitHub issue #18's resume
/// follow-up discussion.
#[tokio::test]
async fn resume_replays_a_spooled_article_under_its_original_message_id() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));

    {
        let posts = posts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(stream, posts.clone()));
            }
        });
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replay.bin");
    std::fs::write(&path, vec![0x99_u8; 50]).unwrap();
    let state_path = dir.path().join("replay.bin.pesto-state");

    let mut config = make_config(addr.port());
    config.resume = true;

    // Simulate an interrupted run that had already encoded and spooled this
    // segment's article (and quite possibly sent it — the ack was lost)
    // before dying, without ever getting far enough to record it in resume
    // state (that only happens once the response comes back).
    let spool_dir = pesto::spool::spool_dir(&state_path);
    pesto::spool::write(
        &spool_dir,
        "replay.bin",
        1,
        "already-sent@spool.test",
        b"Message-ID: <already-sent@spool.test>\r\nSubject: test\r\n",
        b"=ybegin line=128 size=50 name=replay.bin\r\nfake-body\r\n=yend size=50 crc32=00000000\r\n",
    )
    .await
    .unwrap();

    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "replay.bin".to_string(),
    }];
    let outcome = post_files_with_progress(&config, &inputs, None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), 1);
    assert_eq!(
        outcome.segments[0].message_id, "already-sent@spool.test",
        "the spooled Message-ID must be reused verbatim, not a freshly generated one"
    );
    assert_eq!(
        posts.load(Ordering::Relaxed),
        1,
        "the spooled article must actually be (re)sent to the server, not skipped"
    );
    // The spool entry is consumed once the replay is confirmed.
    assert!(pesto::spool::read(&spool_dir, "replay.bin", 1).is_none());
}
