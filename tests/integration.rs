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
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_memory_limit: Some(1_000_000_000),
        par2_only: false,
        extra_servers: vec![],
        verify: false,
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        indexer_url: None,
        indexer_api_key: None,
        indexer_category: None,
        no_upload: false,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: true,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        message_id_domain: None,
        post_hook: None,
        no_hooks: false,
        nfo: false,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 30,
        check_retries: 2,
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
        &config.from,
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
        false,
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
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_memory_limit: Some(1_000_000_000),
        par2_only: false,
        extra_servers: vec![],
        verify: false,
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        indexer_url: None,
        indexer_api_key: None,
        indexer_category: None,
        no_upload: false,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: false,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        message_id_domain: None,
        post_hook: None,
        no_hooks: false,
        nfo: false,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 30,
        check_retries: 2,
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
    state.record("resume_test.bin", 1, "seg1@preposted.example");
    state.record("resume_test.bin", 2, "seg2@preposted.example");
    state.record("resume_test.bin", 3, "seg3@preposted.example");
    state.save(&state_path).unwrap();

    let mut config = make_config(addr.port());
    config.resume = true;

    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "resume_test.bin".to_string(),
    }];
    let outcome = post_files_with_progress(&config, &inputs, None, Some(&state_path))
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
