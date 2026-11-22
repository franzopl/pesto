//! Targeted regression tests for encrypted resume invalidation and identity:
//! - T13 / PE-NEXT-1: ordered source identity rotates salt on file replacement
//! - T14 / PE-NEXT-2: retain current fingerprints after invalidation
//! - T15 / PE-NEXT-3: encryption-to-ordinary spool transition clears spool

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

mod support;
use pesto::config::{Config, FileConfig, ObfuscateMode, Overrides};
use pesto::crypto::extract_bootstrap_from_line1;
use pesto::poster::post_files_with_progress;
use pesto::resume::{FileFingerprint, ResumeState};
use pesto::walk::InputFile;
use support::{fixture_kdf_input, other_kdf_input};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Mock NNTP server that records every attempted article (both accepted and rejected).
struct MockServer {
    port: u16,
    captured: Arc<Mutex<Vec<Vec<u8>>>>,
    accept_count: Arc<Mutex<usize>>,
    max_accept: Arc<Mutex<usize>>,
    close_signal: Arc<AtomicBool>,
}

impl MockServer {
    async fn spawn(max_accept: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let accept_count = Arc::new(Mutex::new(0));
        let max_accept = Arc::new(Mutex::new(max_accept));
        let close_signal = Arc::new(AtomicBool::new(false));

        let server_captured = Arc::clone(&captured);
        let server_accept = Arc::clone(&accept_count);
        let server_max = Arc::clone(&max_accept);
        let server_close = Arc::clone(&close_signal);

        tokio::spawn(async move {
            loop {
                if server_close.load(Ordering::Relaxed) {
                    break;
                }
                tokio::select! {
                    accept_res = listener.accept() => {
                        match accept_res {
                            Ok((stream, _)) => {
                                tokio::spawn(handle_conn(
                                    stream,
                                    Arc::clone(&server_captured),
                                    Arc::clone(&server_accept),
                                    Arc::clone(&server_max),
                                ));
                            }
                            Err(_) => break,
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                }
            }
        });

        Self {
            port,
            captured,
            accept_count,
            max_accept,
            close_signal,
        }
    }

    fn set_max_accept(&self, max: usize) {
        *self.max_accept.lock().unwrap() = max;
    }

    fn captured_articles(&self) -> Vec<Vec<u8>> {
        self.captured.lock().unwrap().clone()
    }

    fn clear_captured(&self) {
        self.captured.lock().unwrap().clear();
        *self.accept_count.lock().unwrap() = 0;
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.close_signal.store(true, Ordering::Relaxed);
    }
}

async fn handle_conn(
    stream: TcpStream,
    captured: Arc<Mutex<Vec<Vec<u8>>>>,
    accept_count: Arc<Mutex<usize>>,
    max_accept: Arc<Mutex<usize>>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let _ = write_half.write_all(b"200 mock ready\r\n").await;

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end();
        if command == "POST" {
            let _ = write_half.write_all(b"340 send article\r\n").await;
            let mut article = Vec::new();
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
                if raw.starts_with(b"..") {
                    raw.remove(0);
                }
                article.extend_from_slice(&raw);
            }
            captured.lock().unwrap().push(article);

            let should_accept = {
                let mut count = accept_count.lock().unwrap();
                let limit = *max_accept.lock().unwrap();
                if *count < limit {
                    *count += 1;
                    true
                } else {
                    false
                }
            };
            if should_accept {
                let _ = write_half.write_all(b"240 article received\r\n").await;
            } else {
                let _ = write_half.write_all(b"441 posting failed\r\n").await;
            }
        } else if command.starts_with("STAT") {
            let _ = write_half.write_all(b"223 0 <id> article exists\r\n").await;
        } else if command.starts_with("MODE READER") {
            let _ = write_half.write_all(b"200 reader mode\r\n").await;
        } else if command == "QUIT" {
            let _ = write_half.write_all(b"205 bye\r\n").await;
            return;
        } else {
            let _ = write_half.write_all(b"500 unknown\r\n").await;
        }
    }
}

fn article_body(article: &[u8]) -> &[u8] {
    if let Some(pos) = article.windows(4).position(|w| w == b"\r\n\r\n") {
        &article[pos + 4..]
    } else {
        article
    }
}

fn make_config(port: u16, encrypt_password: Option<String>, article_size: usize) -> Config {
    let mut file = FileConfig::default();
    file.server.host = Some("127.0.0.1".into());
    file.server.port = Some(port);
    file.server.ssl = Some(false);
    file.server.connections = Some(1);
    file.posting.groups = Some(vec!["alt.test".into()]);
    file.posting.article_size = Some(article_size);
    file.posting.check = Some(false);
    file.posting.retries = Some(1);
    let mut config = Config::resolve(
        file,
        Overrides {
            dry_run: Some(false),
            par2: Some(0),
            encrypt_password,
            ..Default::default()
        },
    )
    .unwrap();
    config.retry_delay = 0;
    config.history = false;
    config.no_hooks = true;
    config.obfuscate = ObfuscateMode::None;
    config.resume = true;
    config
}

/// T13 / PE-NEXT-1: Replacing a source file with a different file of identical
/// size and part count MUST rotate the session salt and discard old records/spool,
/// preventing nonce reuse on the Usenet wire across old and new plaintext.
#[tokio::test]
async fn test_pe_next_1_ordered_source_identity_rotates_salt_on_file_replacement() {
    let server = MockServer::spawn(1).await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("upload.pesto-state");

    let a_path = dir.path().join("a.bin");
    let b_path = dir.path().join("b.bin");
    std::fs::write(&a_path, vec![0x11u8; 64]).unwrap();
    std::fs::write(&b_path, vec![0x22u8; 64]).unwrap();

    let config = make_config(server.port, Some(fixture_kdf_input().to_string()), 32);

    let files_run1 = vec![
        InputFile {
            path: a_path.clone(),
            name: "a.bin".into(),
        },
        InputFile {
            path: b_path.clone(),
            name: "b.bin".into(),
        },
    ];

    // Run 1: accepts 1st article (a.bin part 1), rejects the rest.
    // Leaves incomplete state on disk.
    let outcome1 = post_files_with_progress(&config, &files_run1, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(!outcome1.failures.is_empty(), "run 1 must have failures");
    assert!(state_path.exists(), "run 1 must persist resume state");

    let state1 = ResumeState::load(&state_path).unwrap();
    let salt1 = *state1
        .session_salt()
        .expect("run 1 must record session salt");

    // Capture attempted articles from Run 1: includes attempted b.bin article under salt1.
    let run1_attempts = server.captured_articles();
    assert!(
        run1_attempts.len() >= 2,
        "run 1 must have attempted multiple articles"
    );

    // Replace b.bin with c.bin: same size 64 bytes, same part count (2 parts), different content.
    let c_path = dir.path().join("c.bin");
    std::fs::write(&c_path, vec![0x33u8; 64]).unwrap();

    let files_run2 = vec![
        InputFile {
            path: a_path.clone(),
            name: "a.bin".into(),
        },
        InputFile {
            path: c_path.clone(),
            name: "c.bin".into(),
        },
    ];

    server.clear_captured();
    server.set_max_accept(100);

    // Run 2: resume upload with a.bin and c.bin.
    let outcome2 = post_files_with_progress(&config, &files_run2, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(
        outcome2.failures.is_empty(),
        "run 2 must complete without failures"
    );

    let run2_articles = server.captured_articles();
    assert!(
        !run2_articles.is_empty(),
        "run 2 must have posted wire articles"
    );

    let (salt2, _) =
        extract_bootstrap_from_line1(article_body(&run2_articles[0])).expect("line 1 bootstrap");

    // Defect reproduction: count-only ReleaseLayout matched, so salt1 == salt2.
    // Correct behavior: salt MUST rotate on source identity change!
    assert_ne!(
        salt1, salt2,
        "T13: salt MUST rotate when source file is replaced with same part-count layout"
    );
}

/// T14 / PE-NEXT-2: Invalidation must retain the complete current set of
/// file fingerprints so subsequent runs detect modifications and prevent
/// old/new hybrid uploads.
#[tokio::test]
async fn test_pe_next_2_retain_current_fingerprints_after_invalidation() {
    let server = MockServer::spawn(1).await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("upload.pesto-state");

    let a_path = dir.path().join("a.bin");
    let b_path = dir.path().join("b.bin");
    std::fs::write(&a_path, vec![0x11u8; 64]).unwrap();
    std::fs::write(&b_path, vec![0x22u8; 64]).unwrap();

    let config = make_config(server.port, Some(fixture_kdf_input().to_string()), 32);

    let files = vec![
        InputFile {
            path: a_path.clone(),
            name: "a.bin".into(),
        },
        InputFile {
            path: b_path.clone(),
            name: "b.bin".into(),
        },
    ];

    // Run 0: accepts 1st article, rejects rest -> saves state.
    let _ = post_files_with_progress(&config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(state_path.exists());

    // Before Run 1: modify b.bin content and advance mtime by 100 seconds.
    std::fs::write(&b_path, vec![0x44u8; 64]).unwrap();
    let file = std::fs::File::open(&b_path).unwrap();
    let advanced_mtime = SystemTime::now() + Duration::from_secs(100);
    file.set_modified(advanced_mtime).unwrap();
    drop(file);

    server.clear_captured();
    server.set_max_accept(1);

    // Run 1: detects b.bin change, rotates salt, accepts 1 article, rejects rest -> saves state.
    let _ = post_files_with_progress(&config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(state_path.exists());

    let state1 = ResumeState::load(&state_path).unwrap();

    // Defect reproduction: prepare_resources called invalidate_session() late,
    // wiping state1.files to {}.
    // Correct behavior: intermediate state MUST retain all current file fingerprints!
    assert!(
        !state1.is_empty() || state1.session_salt().is_some(),
        "intermediate state must exist"
    );
    let fp_a = FileFingerprint {
        size: 64,
        mtime: std::fs::metadata(&a_path)
            .unwrap()
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
    };
    let fp_b = FileFingerprint {
        size: 64,
        mtime: std::fs::metadata(&b_path)
            .unwrap()
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
    };

    assert!(
        state1.file_matches("a.bin", &fp_a)
            && state1.file_matches("b.bin", &fp_b)
            && state1.get("a.bin", 1).is_some(),
        "intermediate state must retain current records"
    );
    // Explicitly check that files is NOT empty in serialized state.
    let state_text = std::fs::read_to_string(&state_path).unwrap();
    assert!(
        !state_text.contains("\"files\":{}"),
        "T14: saved intermediate state must NOT have empty files map: {}",
        state_text
    );
    let salt1 = *state1
        .session_salt()
        .expect("intermediate state must have salt");

    // Before Run 2: change a.bin bytes without changing size and advance mtime by 200s.
    std::fs::write(&a_path, vec![0x55u8; 64]).unwrap();
    let file = std::fs::File::open(&a_path).unwrap();
    let advanced_mtime_2 = SystemTime::now() + Duration::from_secs(200);
    file.set_modified(advanced_mtime_2).unwrap();
    drop(file);

    server.clear_captured();
    server.set_max_accept(100);

    // Run 2: because intermediate state preserved a.bin's fingerprint, this run
    // MUST detect that a.bin changed, rotate the salt, clear stale accepted segments
    // from Run 1, and re-post everything cleanly without producing an old/new hybrid.
    let outcome2 = post_files_with_progress(&config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(outcome2.failures.is_empty(), "run 2 must complete");

    let run2_articles = server.captured_articles();
    assert_eq!(
        run2_articles.len(),
        4,
        "run 2 must post all 4 segments fresh (no stale accepted segments skipped)"
    );

    let (salt2, _) =
        extract_bootstrap_from_line1(article_body(&run2_articles[0])).expect("run 2 bootstrap");
    assert_ne!(
        salt1, salt2,
        "T14: salt MUST rotate on second content change rather than skipping stale segments"
    );
}

/// T15 / PE-NEXT-3: Switching from encrypted upload to ordinary upload on resume
/// MUST discard the encrypted spool and post genuine ordinary yEnc articles
/// into an unmarked NZB.
#[tokio::test]
async fn test_pe_next_3_encryption_to_ordinary_spool_transition_clears_spool() {
    let server = MockServer::spawn(0).await; // Reject all posts so spool files are retained
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("upload.pesto-state");
    let spool_dir = pesto::spool::spool_dir(&state_path);

    let test_path = dir.path().join("test.bin");
    std::fs::write(&test_path, vec![0x99u8; 64]).unwrap();

    let files = vec![InputFile {
        path: test_path.clone(),
        name: "test.bin".into(),
    }];

    // Run 0: Encrypted upload, rejects all posts.
    let enc_config = make_config(server.port, Some(fixture_kdf_input().to_string()), 32);
    let outcome0 = post_files_with_progress(&enc_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(!outcome0.failures.is_empty());
    assert!(state_path.exists());
    assert!(spool_dir.exists(), "encrypted spool directory must exist");

    // Check that spool contains encrypted data (starts with bootstrap, NOT =ybegin)
    let spooled = pesto::spool::read(&spool_dir, "test.bin", 1).expect("spool entry 1 exists");
    assert!(
        !spooled.body.starts_with(b"=ybegin "),
        "spooled body must be encrypted"
    );

    server.clear_captured();
    server.set_max_accept(100);

    // Run 1: Resume as an ORDINARY upload (encrypt_password = None).
    let mut ord_config = make_config(server.port, None, 32);
    ord_config.resume = true;

    let outcome1 = post_files_with_progress(&ord_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(
        outcome1.failures.is_empty(),
        "ordinary resume must succeed: {:?}",
        outcome1.failures
    );

    let posted_articles = server.captured_articles();
    assert_eq!(posted_articles.len(), 2, "both parts must be posted");

    // Defect reproduction: ordinary resume replayed encrypted spool body!
    // Correct behavior: articles posted MUST be genuine ordinary yEnc starting with =ybegin!
    let body0 = article_body(&posted_articles[0]);
    assert!(
        body0.starts_with(b"=ybegin "),
        "T15: ordinary resume must post genuine yEnc articles starting with =ybegin, got: {:?}",
        String::from_utf8_lossy(&body0[..40.min(body0.len())])
    );
    let body1 = article_body(&posted_articles[1]);
    assert!(body1.starts_with(b"=ybegin "));
    // The NZB must carry no encryption marker (ordinary upload semantics).
    let nzb_meta = pesto::nzb::NzbMeta {
        yenc_encrypted: false,
        ..Default::default()
    };
    let xml = pesto::nzb::generate(
        &outcome1.groups,
        &outcome1.segments,
        &nzb_meta,
        ObfuscateMode::None,
    );
    assert!(
        !xml.contains("yenc_encrypted"),
        "T15: ordinary resume must emit an unmarked NZB, got: {xml}"
    );
    assert!(
        !spool_dir.exists(),
        "T15: stale encrypted spool must be discarded on ordinary resume"
    );
}

/// T15 control: ordinary → encrypted transition also discards incompatible
/// (ordinary) spool, and a password change rotates identity.
#[tokio::test]
async fn test_pe_next_3_ordinary_to_encrypted_transition_discards_incompatible_spool() {
    let server = MockServer::spawn(0).await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("upload.pesto-state");
    let spool_dir = pesto::spool::spool_dir(&state_path);

    let test_path = dir.path().join("test.bin");
    std::fs::write(&test_path, vec![0x99u8; 64]).unwrap();

    let files = vec![InputFile {
        path: test_path.clone(),
        name: "test.bin".into(),
    }];

    // Run 0: Ordinary upload, rejects all posts -> ordinary spool retained.
    let ord_config = make_config(server.port, None, 32);
    let outcome0 = post_files_with_progress(&ord_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(!outcome0.failures.is_empty());
    assert!(spool_dir.exists());
    let spooled = pesto::spool::read(&spool_dir, "test.bin", 1).unwrap();
    assert!(spooled.body.starts_with(b"=ybegin "));

    server.clear_captured();
    server.set_max_accept(100);

    // Run 1: Resume as an ENCRYPTED upload. The ordinary spool entries are
    // incompatible with the encrypted session and must be discarded, not replayed.
    let enc_config = make_config(server.port, Some("newpw".into()), 32);
    let outcome1 = post_files_with_progress(&enc_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(outcome1.failures.is_empty());

    let posted = server.captured_articles();
    assert_eq!(posted.len(), 2);
    for article in &posted {
        let (salt, index) =
            extract_bootstrap_from_line1(article_body(article)).expect("encrypted bootstrap");
        assert!(index > 0);
        assert_ne!([0u8; 16], salt);
        // Encrypted article bodies never start with =ybegin (bootstrap first).
        assert!(!article_body(article).starts_with(b"=ybegin "));
    }
    // The NZB must carry the encryption marker.
    let nzb_meta = pesto::nzb::NzbMeta {
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml = pesto::nzb::generate(
        &outcome1.groups,
        &outcome1.segments,
        &nzb_meta,
        ObfuscateMode::None,
    );
    assert!(xml.contains("yenc_encrypted"), "got: {xml}");
}

/// PU12-01 (Q3(a)): state file removed, spool retained — plain spool must
/// not be replayed into an encrypted run (confidentiality break /
/// undecryptable NZB), and an encrypted spool must not be replayed into an
/// ordinary run. Both directions post freshly encoded articles instead.
#[tokio::test]
async fn orphaned_spool_without_state_is_not_replayed_plain_to_encrypted() {
    let server = MockServer::spawn(0).await; // reject all: spool retained
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("upload.pesto-state");
    let spool_dir = pesto::spool::spool_dir(&state_path);

    let test_path = dir.path().join("test.bin");
    std::fs::write(&test_path, vec![0x77u8; 64]).unwrap();
    let files = vec![InputFile {
        path: test_path.clone(),
        name: "test.bin".into(),
    }];

    // Run 0: ordinary upload, all posts rejected -> plain spool retained.
    let ord_config = make_config(server.port, None, 32);
    let outcome0 = post_files_with_progress(&ord_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(!outcome0.failures.is_empty());
    assert!(spool_dir.exists(), "ordinary spool must be retained");
    let spooled = pesto::spool::read(&spool_dir, "test.bin", 1).unwrap();
    assert!(spooled.body.starts_with(b"=ybegin "));

    // Operator deletes/corrupts the state file but keeps the spool.
    std::fs::remove_file(&state_path).unwrap();
    assert!(!state_path.exists());
    assert!(spool_dir.exists());

    server.clear_captured();
    server.set_max_accept(100);

    // Run 1: encrypted resume. The orphaned plain spool must be cleared /
    // refused, and genuinely encrypted articles posted.
    let enc_config = make_config(server.port, Some("orphanpw".into()), 32);
    let outcome1 = post_files_with_progress(&enc_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(
        outcome1.failures.is_empty(),
        "encrypted resume over orphaned spool must succeed: {:?}",
        outcome1.failures
    );
    let posted = server.captured_articles();
    assert_eq!(posted.len(), 2, "both parts freshly posted");
    for article in &posted {
        let body = article_body(article);
        assert!(
            !body.starts_with(b"=ybegin "),
            "PU12-01: plain spool body must never be replayed into an encrypted run"
        );
        let (salt, index) = extract_bootstrap_from_line1(body).expect("encrypted bootstrap");
        assert!(index > 0);
        assert_ne!(salt, [0u8; 16]);
    }
}

#[tokio::test]
async fn orphaned_spool_without_state_is_not_replayed_encrypted_to_plain() {
    let server = MockServer::spawn(0).await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("upload.pesto-state");
    let spool_dir = pesto::spool::spool_dir(&state_path);

    let test_path = dir.path().join("test.bin");
    std::fs::write(&test_path, vec![0x88u8; 64]).unwrap();
    let files = vec![InputFile {
        path: test_path.clone(),
        name: "test.bin".into(),
    }];

    // Run 0: encrypted upload, all rejected -> encrypted spool retained.
    let enc_config = make_config(server.port, Some("orphanpw2".into()), 32);
    let outcome0 = post_files_with_progress(&enc_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(!outcome0.failures.is_empty());
    assert!(spool_dir.exists());
    let spooled = pesto::spool::read(&spool_dir, "test.bin", 1).unwrap();
    assert!(!spooled.body.starts_with(b"=ybegin "));

    std::fs::remove_file(&state_path).unwrap();

    server.clear_captured();
    server.set_max_accept(100);

    // Run 1: ordinary resume. Ciphertext spool must not be replayed.
    let ord_config = make_config(server.port, None, 32);
    let outcome1 = post_files_with_progress(&ord_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(outcome1.failures.is_empty());
    let posted = server.captured_articles();
    assert_eq!(posted.len(), 2);
    for article in &posted {
        assert!(
            article_body(article).starts_with(b"=ybegin "),
            "PU12-01: encrypted spool body must never be replayed into an ordinary run"
        );
    }
}

/// PU12-01 stamp half: with the state file present, a spool entry stamped
/// under a different salt is refused on replay even if the spool directory
/// survived session invalidation (defence in depth behind the clear).
#[tokio::test]
async fn stamped_spool_mismatch_is_refused_when_state_present() {
    let server = MockServer::spawn(0).await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("upload.pesto-state");

    let test_path = dir.path().join("test.bin");
    std::fs::write(&test_path, vec![0x55u8; 64]).unwrap();
    let files = vec![InputFile {
        path: test_path.clone(),
        name: "test.bin".into(),
    }];

    // Run 0: encrypted upload (password A), all rejected.
    let config_a = make_config(server.port, Some(fixture_kdf_input().to_string()), 32);
    let _ = post_files_with_progress(&config_a, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(state_path.exists());
    let salt_a = *ResumeState::load(&state_path)
        .unwrap()
        .session_salt()
        .expect("salt A");

    server.clear_captured();
    server.set_max_accept(100);

    // Run 1: same state file, different encryption password. The fingerprint
    // mismatch invalidates the session (new salt), and any spool entry from
    // run 0 is stamped with salt A — replay must refuse it and post fresh
    // articles under the new salt.
    let config_b = make_config(server.port, Some(other_kdf_input().into()), 32);
    let outcome1 = post_files_with_progress(&config_b, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(outcome1.failures.is_empty());
    let posted = server.captured_articles();
    assert_eq!(posted.len(), 2);
    for article in &posted {
        let (salt, _) =
            extract_bootstrap_from_line1(article_body(article)).expect("encrypted bootstrap");
        assert_ne!(salt, salt_a, "rotated session must not replay salt-A spool");
    }
}

/// Unchanged encrypted retry retains the exact salt, index, and wire bytes.
#[tokio::test]
async fn test_pe_unchanged_encrypted_retry_preserves_wire_identity() {
    let server = MockServer::spawn(1).await;
    let dir = tempfile::tempdir().unwrap();
    let state_path = dir.path().join("upload.pesto-state");

    let test_path = dir.path().join("test.bin");
    std::fs::write(&test_path, vec![0x42u8; 64]).unwrap();

    let files = vec![InputFile {
        path: test_path.clone(),
        name: "test.bin".into(),
    }];

    // Run 0: accepts part 1, fails part 2.
    let enc_config = make_config(server.port, Some("stablepw".into()), 32);
    let _ = post_files_with_progress(&enc_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(state_path.exists());

    let state0 = ResumeState::load(&state_path).unwrap();
    let salt0 = *state0.session_salt().expect("salt recorded");
    let rec1 = state0.get("test.bin", 1).expect("part 1 recorded").clone();

    server.clear_captured();
    server.set_max_accept(100);

    // Run 1: resume unchanged. Part 1 should be skipped; part 2 posted under salt0.
    let outcome1 = post_files_with_progress(&enc_config, &files, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(outcome1.failures.is_empty());

    let run1_articles = server.captured_articles();
    assert_eq!(run1_articles.len(), 1, "part 1 skipped, only part 2 posted");

    let (salt1, idx2) =
        extract_bootstrap_from_line1(article_body(&run1_articles[0])).expect("part 2 bootstrap");
    assert_eq!(salt0, salt1, "salt must be preserved on unchanged resume");
    assert_eq!(idx2, 2, "part 2 must receive segment index 2");

    // The completed outcome preserves part 1's original message ID.
    let seg1 = outcome1
        .segments
        .iter()
        .find(|s| s.part == 1)
        .expect("part 1 in outcome");
    assert_eq!(seg1.message_id, rec1.message_id);
}
