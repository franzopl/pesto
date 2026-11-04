//! Regression guards: every repost path of an encrypted upload
//! must re-post the *authenticated wire form* — re-encrypted with the same
//! salt and segment index — never plaintext yEnc (Body Encryption Standard
//! v1.2 §8).
//!
//! Covers both paths:
//! - the end-of-run blind retry (`poster::repost_failed_tasks`): the mock
//!   server rejects the first POST attempts with 441, forcing the failed
//!   task into the retry pass;
//! - the streaming STAT check's repost (`poster::check::repost_one`) and the
//!   final recovery pass (`poster::check::recover_missing`), which both run
//!   through the same `repost_one`.
//!
//! Each test captures the raw bytes of every POST the server received,
//! decrypts them through the production `DownloadDecryptionAdapter`, and
//! asserts the reconstructed plaintext equals the source file. No captured
//! article body may start with `=ybegin` (plaintext) while the upload is
//! encrypted.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

mod support;
use pesto::config::{Config, ObfuscateMode};
use pesto::crypto::DownloadDecryptionAdapter;
use pesto::poster::post_files_with_progress;
use pesto::walk::expand_inputs;
use support::fixture_kdf_input;

type CapturedArticles = Arc<Mutex<Vec<Vec<u8>>>>;

/// Withholds one NNTP reply until the test releases it, after the gate
/// command's raw bytes have been fully captured. Lets a test mutate the
/// on-disk source at a deterministic point: after the original POST was
/// observed on the wire, before any later reread-based retry can re-read the
/// file (PESTO-FINAL-1).
struct PostGate {
    /// Fired once the gated command's bytes are fully captured.
    notify: std::sync::mpsc::Sender<()>,
    /// `false` = closed: the captured command's reply is withheld.
    open: Mutex<bool>,
    cv: std::sync::Condvar,
}

impl PostGate {
    /// Called on the mock-server thread: signal the test, then block until
    /// the test calls [`PostGate::release`].
    fn reply_until_released(&self) {
        let _ = self.notify.send(());
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.cv.wait(open).unwrap();
        }
    }

    /// Called on the test thread: let the withheld reply through.
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

/// Wait (bounded) for the mock server to signal that the gated command's
/// bytes were captured.
fn wait_captured(gate_signal: &std::sync::mpsc::Receiver<()>) {
    gate_signal
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("mock server never captured the gated command");
}

/// Flip the first byte of the file in place, preserving its length — a
/// same-length source mutation, exactly the reread-identity hazard.
fn flip_first_byte(path: &std::path::Path) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[0] ^= 0xFF;
    std::fs::write(path, &bytes).unwrap();
}

/// Whether an article's decoded yEnc body decrypts (via the production
/// adapter) to `plaintext`. Test helper for the changed-source proofs.
fn decrypts_to(body: &[u8], password: &str, plaintext: &[u8]) -> bool {
    let decryptor = DownloadDecryptionAdapter::with_password(password);
    decryptor
        .decode_article(body)
        .map(|d| d.data == plaintext)
        .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_blind_retry_refuses_changed_source_under_same_identity() {
    // PESTO-FINAL-1: a source that changes AFTER the first attempted POST
    // must never be blind-retried under the original salt/segment index —
    // that would put two distinct plaintexts under one authenticated
    // identity (Body Encryption Standard v1.2 §8). The gate holds the
    // server's 441 reply until the source has been mutated: the original
    // POST is fully captured (and refused), the blind retry has not run yet.
    let (addr, articles, refused, _refused_articles, gate, signal) =
        spawn_mock_server_with_post_gate(1, 1);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("changed-source.bin");
    let plaintext: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    std::fs::write(&input, &plaintext).unwrap();

    let cfg = config(addr, Some("conformance-fixture-changed-password".into()));
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let run =
        tokio::spawn(
            async move { post_files_with_progress(&cfg, &inputs, None, None, None).await },
        );

    wait_captured(&signal);
    flip_first_byte(&input);
    let mutated = std::fs::read(&input).unwrap();
    gate.release();

    let outcome = run.await.unwrap().unwrap();

    let captured = articles.lock().unwrap().clone();
    if !captured.is_empty() {
        let body = article_body(&captured[0]);
        panic!(
            "PESTO-FINAL-1 RED: the changed source was re-read, re-encrypted under the \
             same salt/segment index and posted — the server accepted {} article(s); \
             decoded retry plaintext equals mutated source: {}",
            captured.len(),
            decrypts_to(body, "conformance-fixture-changed-password", &mutated)
        );
    }
    assert_eq!(
        refused.load(Ordering::SeqCst),
        1,
        "the blind retry must not issue a second POST for a changed source"
    );
    assert!(
        !outcome.failures.is_empty(),
        "the segment must stay failed when the retry is refused: {:?}",
        outcome.failures
    );
    assert!(
        !outcome.failed_tasks.is_empty(),
        "the refused segment must remain a failed task"
    );
    assert!(
        outcome.segments.is_empty(),
        "no successful segment may be recorded for a changed source"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_check_repost_refuses_changed_source_under_same_identity() {
    // PESTO-FINAL-1 through the STAT path: all POSTs succeed, but the first
    // STAT's 430 reply is held until the source has been mutated — the
    // `repost_one` reread (used by both the streaming check repost
    // (`check.rs` `handle_confirmed_miss`) and the final recovery pass
    // (`recovery.rs`)) has not run yet. The repost must be refused, leaving
    // the segment missing rather than posting the mutated bytes.
    let (addr, articles, stat_count, gate, signal) = spawn_mock_server_with_stat_gate();

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("changed-check.bin");
    let plaintext: Vec<u8> = (0..96u32).map(|i| (i * 5) as u8).collect();
    std::fs::write(&input, &plaintext).unwrap();

    let cfg = config(addr, Some("conformance-fixture-changed-password".into()));
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let run =
        tokio::spawn(
            async move { post_files_with_progress(&cfg, &inputs, None, None, None).await },
        );

    wait_captured(&signal);
    flip_first_byte(&input);
    let mutated = std::fs::read(&input).unwrap();
    gate.release();

    let outcome = run.await.unwrap().unwrap();

    let captured = articles.lock().unwrap().clone();
    if captured.len() > 1 {
        let body = article_body(&captured[1]);
        panic!(
            "PESTO-FINAL-1 RED: the changed source was re-read by the STAT repost and \
             posted under the same salt/segment index — the server holds {} article(s); \
             decoded repost plaintext equals mutated source: {}",
            captured.len(),
            decrypts_to(body, "conformance-fixture-changed-password", &mutated)
        );
    }
    assert_eq!(
        captured.len(),
        1,
        "only the original POST may be stored; the changed-source repost must be refused"
    );
    assert_eq!(
        stat_count.load(Ordering::SeqCst),
        1,
        "no confirmation STAT may run for a refused repost"
    );
    let body = article_body(&captured[0]);
    let decryptor =
        DownloadDecryptionAdapter::with_password("conformance-fixture-changed-password");
    let decoded = decryptor
        .decode_article(body)
        .expect("the original stored article must decrypt");
    assert_eq!(
        decoded.data, plaintext,
        "the only stored article must be the original post of the unmutated source"
    );
    assert!(
        !outcome.inconclusive.is_empty() || !outcome.still_missing.is_empty(),
        "the refused repost must end the segment not-posted (missing/inconclusive), \
         never silently successful: still_missing={:?} inconclusive={:?}",
        outcome.still_missing,
        outcome.inconclusive
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_blind_retry_still_rereads_current_source_bytes() {
    // Control (PESTO-FINAL-1 2d): WITHOUT an encryption adapter there is no
    // salt/segment identity to protect, and the reread-based retry's
    // established upstream behavior is to post whatever is currently on disk
    // (the retry has always re-read at retry time — issue #23/#145 lineage).
    // No digest is computed and no gate applies: ordinary retries keep
    // posting the current bytes.
    let (addr, articles, refused, _refused_articles, gate, signal) =
        spawn_mock_server_with_post_gate(1, 1);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("plain-changed.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();

    let cfg = config(addr, None);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let run =
        tokio::spawn(
            async move { post_files_with_progress(&cfg, &inputs, None, None, None).await },
        );

    wait_captured(&signal);
    flip_first_byte(&input);
    let mutated = std::fs::read(&input).unwrap();
    gate.release();

    let outcome = run.await.unwrap().unwrap();

    assert!(
        outcome.failures.is_empty(),
        "the ordinary blind retry must still recover the failure: {:?}",
        outcome.failures
    );
    let captured = articles.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        1,
        "the ordinary blind retry must still POST (upstream behavior unchanged)"
    );
    assert_eq!(
        refused.load(Ordering::SeqCst),
        2,
        "the ordinary retry issues its own POST (refused original + accepted retry)"
    );
    let decoded = pesto::yenc::decode_part(article_body(&captured[0])).unwrap();
    assert_eq!(
        decoded.data, mutated,
        "ordinary uploads must keep their existing reread-based retry behavior \
         (no hashing, no identity gate)"
    );
}

/// Mock NNTP server. `post_failures` controls how many POST commands (across
/// all connections, globally) are answered with a 441 refusal before any
/// later POST succeeds. All STATs succeed, so no check-path repost
/// interferes with the blind-retry assertions.
///
/// Refused articles are captured in `refused_articles` too: the blind-retry
/// scenario compares the refused initial attempt's raw bytes against the
/// accepted retry's bytes to prove identity stability.
fn spawn_mock_server(
    post_failures: usize,
) -> (
    SocketAddr,
    CapturedArticles,
    Arc<AtomicUsize>,
    CapturedArticles,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let articles = Arc::new(Mutex::new(Vec::new()));
    let refused_articles: CapturedArticles = Arc::new(Mutex::new(Vec::new()));
    let refused = Arc::new(AtomicUsize::new(0));

    let articles_clone = Arc::clone(&articles);
    let refused_articles_clone = Arc::clone(&refused_articles);
    let refused_clone = Arc::clone(&refused);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let articles = Arc::clone(&articles_clone);
            let refused_articles = Arc::clone(&refused_articles_clone);
            let refused = Arc::clone(&refused_clone);
            std::thread::spawn(move || {
                handle_connection(stream, articles, refused_articles, refused, post_failures);
            });
        }
    });

    (addr, articles, refused, refused_articles)
}

/// Like [`spawn_mock_server`], but the first `stat_misses` STAT commands
/// report 430 (missing) and every later STAT succeeds — the recipe that
/// drives a normal check repost plus the automatic recovery-pass repost
/// (see `check_recover_pass.rs`).
fn spawn_mock_server_with_stat_misses(
    stat_misses: usize,
) -> (SocketAddr, CapturedArticles, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let articles = Arc::new(Mutex::new(Vec::new()));
    let stat_count = Arc::new(AtomicUsize::new(0));

    let articles_clone = Arc::clone(&articles);
    let stat_count_clone = Arc::clone(&stat_count);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let articles = Arc::clone(&articles_clone);
            let stat_count = Arc::clone(&stat_count_clone);
            std::thread::spawn(move || {
                handle_connection_with_stat_misses(stream, articles, stat_count, stat_misses);
            });
        }
    });

    (addr, articles, stat_count)
}

fn handle_connection(
    stream: TcpStream,
    articles: CapturedArticles,
    refused_articles: CapturedArticles,
    refused: Arc<AtomicUsize>,
    post_failures: usize,
) {
    handle_connection_gated(
        stream,
        articles,
        refused_articles,
        refused,
        post_failures,
        None,
    );
}

/// Like [`spawn_mock_server`], but the `gate_at`-th POST command (1-based,
/// counting refused+accepted alike) has its `441`/`240` reply withheld until
/// [`PostGate::release`] — the test mutates the source in that window,
/// between the gated POST's capture and any later reread-based retry.
fn spawn_mock_server_with_post_gate(
    post_failures: usize,
    gate_at: usize,
) -> (
    SocketAddr,
    CapturedArticles,
    Arc<AtomicUsize>,
    CapturedArticles,
    Arc<PostGate>,
    std::sync::mpsc::Receiver<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let articles = Arc::new(Mutex::new(Vec::new()));
    let refused_articles: CapturedArticles = Arc::new(Mutex::new(Vec::new()));
    let refused = Arc::new(AtomicUsize::new(0));
    let (notify, rx) = std::sync::mpsc::channel();
    let gate = Arc::new(PostGate {
        notify,
        open: Mutex::new(false),
        cv: std::sync::Condvar::new(),
    });

    let articles_clone = Arc::clone(&articles);
    let refused_articles_clone = Arc::clone(&refused_articles);
    let refused_clone = Arc::clone(&refused);
    let gate_clone = Arc::clone(&gate);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let articles = Arc::clone(&articles_clone);
            let refused_articles = Arc::clone(&refused_articles_clone);
            let refused = Arc::clone(&refused_clone);
            let gate = Arc::clone(&gate_clone);
            std::thread::spawn(move || {
                handle_connection_gated(
                    stream,
                    articles,
                    refused_articles,
                    refused,
                    post_failures,
                    Some((gate, gate_at)),
                );
            });
        }
    });

    (addr, articles, refused, refused_articles, gate, rx)
}

#[allow(clippy::too_many_arguments)]
fn handle_connection_gated(
    stream: TcpStream,
    articles: CapturedArticles,
    refused_articles: CapturedArticles,
    refused: Arc<AtomicUsize>,
    post_failures: usize,
    gate: Option<(Arc<PostGate>, usize)>,
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
            let this_post = refused.fetch_add(1, Ordering::SeqCst);
            let accept = this_post >= post_failures;
            if writer.write_all(b"340 send article\r\n").is_err() {
                return;
            }
            let mut article = Vec::new();
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw) {
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
            let gated = matches!(&gate, Some((_, at)) if this_post + 1 == *at);
            if gated {
                if let Some((g, _)) = &gate {
                    g.reply_until_released();
                }
            }
            if accept {
                articles.lock().unwrap().push(article);
                if writer.write_all(b"240 article received\r\n").is_err() {
                    return;
                }
            } else {
                refused_articles.lock().unwrap().push(article);
                if writer
                    .write_all(b"441 article rejected (forced test failure)\r\n")
                    .is_err()
                {
                    return;
                }
            }
        } else if command.starts_with("STAT ") {
            if writer.write_all(b"223 0 <id> article exists\r\n").is_err() {
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

/// Like [`spawn_mock_server_with_stat_misses`], but the first STAT's `430`
/// reply is withheld until [`PostGate::release`] — the test mutates the
/// source in that window, before the check repost's reread has run.
fn spawn_mock_server_with_stat_gate() -> (
    SocketAddr,
    CapturedArticles,
    Arc<AtomicUsize>,
    Arc<PostGate>,
    std::sync::mpsc::Receiver<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let articles = Arc::new(Mutex::new(Vec::new()));
    let stat_count = Arc::new(AtomicUsize::new(0));
    let (notify, rx) = std::sync::mpsc::channel();
    let gate = Arc::new(PostGate {
        notify,
        open: Mutex::new(false),
        cv: std::sync::Condvar::new(),
    });

    let articles_clone = Arc::clone(&articles);
    let stat_count_clone = Arc::clone(&stat_count);
    let gate_clone = Arc::clone(&gate);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let articles = Arc::clone(&articles_clone);
            let stat_count = Arc::clone(&stat_count_clone);
            let gate = Arc::clone(&gate_clone);
            std::thread::spawn(move || {
                handle_connection_with_stat_gate(stream, articles, stat_count, gate);
            });
        }
    });

    (addr, articles, stat_count, gate, rx)
}

fn handle_connection_with_stat_gate(
    stream: TcpStream,
    articles: CapturedArticles,
    stat_count: Arc<AtomicUsize>,
    gate: Arc<PostGate>,
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
            let mut article = Vec::new();
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw) {
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
            articles.lock().unwrap().push(article);
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if command.starts_with("STAT ") {
            let seen = stat_count.fetch_add(1, Ordering::SeqCst);
            if seen == 0 {
                gate.reply_until_released();
            }
            let resp: &[u8] = if seen == 0 {
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

fn handle_connection_with_stat_misses(
    stream: TcpStream,
    articles: CapturedArticles,
    stat_count: Arc<AtomicUsize>,
    stat_misses: usize,
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
            let mut article = Vec::new();
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw) {
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
            articles.lock().unwrap().push(article);
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if command.starts_with("STAT ") {
            let seen = stat_count.fetch_add(1, Ordering::SeqCst);
            let resp: &[u8] = if seen < stat_misses {
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

fn config(addr: SocketAddr, encrypt_password: Option<String>) -> Config {
    config_with_obfuscation(addr, encrypt_password, ObfuscateMode::None)
}

fn config_with_obfuscation(
    addr: SocketAddr,
    encrypt_password: Option<String>,
    obfuscate: ObfuscateMode,
) -> Config {
    Config {
        host: addr.ip().to_string(),
        port: addr.port(),
        ssl: false,
        connections: 2,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 1_000_000,
        line_length: 128,
        retries: 1,
        retry_delay: 0,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        proxy: None,
        proxy_check_ip: false,
        obfuscate,
        dry_run: false,
        par2: 0,
        par2_effort: 1.0,
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
        encrypt_password,
        ext: Vec::new(),
        exclude: Vec::new(),
        no_exclude: false,
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

fn article_body(article: &[u8]) -> &[u8] {
    let marker = b"\r\n\r\n";
    let start = article
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("article headers must end with CRLF CRLF")
        + marker.len();
    &article[start..]
}

/// Assert every captured article is an authenticated encrypted wire article
/// whose decrypted bytes equal `plaintext` (single-segment uploads: the
/// original post and every repost each carry the whole file), and return the
/// bootstrap segment indices in capture order.
fn assert_all_articles_decrypt_to(
    articles: &[Vec<u8>],
    password: &str,
    plaintext: &[u8],
) -> Vec<u32> {
    assert!(!articles.is_empty(), "no articles were captured");
    let decryptor = DownloadDecryptionAdapter::with_password(password);
    let mut indices = Vec::new();
    for (i, article) in articles.iter().enumerate() {
        let body = article_body(article);
        assert!(
            !body.starts_with(b"=ybegin"),
            "article #{i} was posted as PLAINTEXT yEnc during an encrypted upload"
        );
        let (salt, index) = pesto::crypto::extract_bootstrap_from_line1(body)
            .unwrap_or_else(|e| panic!("article #{i} lacks a valid bootstrap: {e}"));
        assert_eq!(salt.len(), 16);
        indices.push(index);
        let decoded = decryptor
            .decode_article(body)
            .unwrap_or_else(|e| panic!("article #{i} failed to decrypt: {e}"));
        assert_eq!(
            decoded.data, plaintext,
            "article #{i} must decrypt to the original plaintext"
        );
    }
    indices
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_blind_retry_reposts_encrypted_wire_with_same_identity() {
    // Refuse the very first POST command: the single-segment article fails
    // its only in-run attempt (`retries: 1`), lands in `failed_tasks`, and
    // is re-posted by the end-of-run blind retry
    // (`poster::repost_failed_tasks`). The retry must re-encrypt with the
    // same salt/index — posting plaintext there is the P0 leak this test
    // guards against.
    let (addr, articles, refused, refused_articles) = spawn_mock_server(1);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("retry.bin");
    let plaintext: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    std::fs::write(&input, &plaintext).unwrap();

    let cfg = config(addr, Some(fixture_kdf_input().to_string()));
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files_with_progress(&cfg, &inputs, None, None, None)
        .await
        .unwrap();

    // Two POST commands total: the refused in-run attempt and the accepted
    // blind-retry POST.
    assert_eq!(
        refused.load(Ordering::SeqCst),
        2,
        "expected the refused in-run POST plus the blind retry's POST"
    );
    assert!(
        outcome.failures.is_empty(),
        "the blind retry must recover the forced failure: {:?}",
        outcome.failures
    );

    let captured = articles.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        1,
        "only the successful retry POST should be stored by the server"
    );

    // The single captured article is the retry's output: it must be the
    // authenticated encrypted wire form decrypting to the original bytes.
    let indices = assert_all_articles_decrypt_to(&captured, fixture_kdf_input(), &plaintext);
    // First safe index of the release.
    assert_eq!(indices, vec![1u32]);

    // Identity check: the retry must reuse the failed task's planned
    // segment index — decrypt its bootstrap and compare with the outcome.
    assert_eq!(
        outcome.segments[0].segment_index, indices[0],
        "retry must preserve the original segment identity"
    );

    // PESTO-R6-1: the recovered segment's NZB `bytes` must be the PLAINTEXT
    // segment length (64), not the wire-article length (446). The AAD binds
    // `segmentBytes` to the plaintext, so a wire-length declaration makes
    // Penne reject the otherwise-valid article (SEGMENT_BYTES_MISMATCH).
    assert_eq!(
        outcome.segments[0].bytes,
        plaintext.len() as u64,
        "blind-retry PostedSegment.bytes must be the plaintext length, not wire length"
    );

    // The generated NZB must carry the same plaintext length: serialize the
    // outcome and parse it back (Body Encryption Standard v1.2 §8 requires
    // producers to declare plaintext segment bytes).
    let meta = pesto::nzb::NzbMeta {
        password: Some(fixture_kdf_input().to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml = pesto::nzb::generate(
        &outcome.groups,
        &outcome.segments,
        &meta,
        ObfuscateMode::None,
    );
    let parsed = pesto::nzb::parse(&xml).unwrap();
    assert_eq!(parsed.segments.len(), outcome.segments.len());
    assert_eq!(
        parsed.segments[0].bytes,
        plaintext.len() as u64,
        "parsed NZB segment bytes must equal the plaintext length (64), not wire length (446)"
    );
    assert!(
        xml.contains(&format!("<segment bytes=\"{}\"", plaintext.len())),
        "generated NZB XML must declare bytes=\"{}\"",
        plaintext.len()
    );

    // Identity stability between the initial attempt and the retry: the
    // refused in-run article and the accepted retry article must carry the
    // same ciphertext body (same salt + segment index ⇒ deterministic
    // re-encryption) and the same Message-ID.
    let refused_captured = refused_articles.lock().unwrap().clone();
    assert_eq!(
        refused_captured.len(),
        1,
        "the refused in-run attempt's article must be captured"
    );
    let initial = &refused_captured[0];
    let retry = &captured[0];
    assert_eq!(
        article_body(initial),
        article_body(retry),
        "retry article body must be byte-identical to the initial attempt \
         (same ciphertext, salt and segment index)"
    );
    assert_eq!(
        article_message_id(initial),
        article_message_id(retry),
        "retry must reuse the initial attempt's Message-ID"
    );
    // Sanity: the initial attempt's ciphertext is not plaintext either.
    assert!(
        !article_body(initial).starts_with(b"=ybegin"),
        "the refused initial attempt must already be encrypted wire form"
    );
}

/// Extract the `Message-ID` header value (including angle brackets) from a
/// captured raw article.
fn article_message_id(article: &[u8]) -> String {
    let text = String::from_utf8_lossy(article);
    for line in text.split("\r\n") {
        if let Some(value) = line
            .strip_prefix("Message-ID:")
            .or_else(|| line.strip_prefix("Message-Id:"))
        {
            return value.trim().to_string();
        }
    }
    panic!("captured article has no Message-ID header");
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_check_repost_and_recovery_stay_encrypted_with_same_identity() {
    // All POSTs succeed, but the first two STATs miss — driving one normal
    // `check_post_retries` repost (`repost_one`) plus the automatic
    // recovery-pass repost, exactly like `check_recover_pass.rs`. Both
    // reposts must be re-encrypted under the same salt/index, never
    // plaintext.
    let (addr, articles, stat_count) = spawn_mock_server_with_stat_misses(2);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("check.bin");
    let plaintext: Vec<u8> = (0..96u32).map(|i| (i * 3) as u8).collect();
    std::fs::write(&input, &plaintext).unwrap();

    let cfg = config(addr, Some(fixture_kdf_input().to_string()));
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files_with_progress(&cfg, &inputs, None, None, None)
        .await
        .unwrap();

    assert!(
        outcome.still_missing.is_empty(),
        "the recovery pass should have resolved the stubborn miss: {:?}",
        outcome.still_missing
    );
    assert_eq!(outcome.segments.len(), 1);
    // Original + check repost + recovery-pass repost.
    assert_eq!(
        stat_count.load(Ordering::SeqCst),
        3,
        "expected 2 failing STATs (original + repost) then 1 succeeding"
    );

    let captured = articles.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        3,
        "expected the original POST plus 2 reposts"
    );

    // All three POSTs — original, check repost, recovery repost — must be
    // encrypted wire forms of the same segment, each carrying the same
    // bootstrap segment index and decrypting to the original plaintext.
    let indices = assert_all_articles_decrypt_to(&captured, fixture_kdf_input(), &plaintext);
    assert_eq!(
        indices,
        vec![1u32; 3],
        "every repost must reuse the original segment index"
    );
    assert_eq!(
        outcome.segments[0].segment_index, indices[0],
        "outcome must record the preserved segment identity"
    );
}

/// Extract the restored (decrypted) yEnc `=ybegin name=` value from a
/// captured raw article, via the production decryption path. The `name=` is
/// an AAD input (Body Std v1.2 §2.1), so this is the authenticated name.
fn restored_yenc_name(article: &[u8], password: &str) -> String {
    let body = article_body(article);
    let decryptor = DownloadDecryptionAdapter::with_password(password);
    let decoded = decryptor
        .decode_article(body)
        .unwrap_or_else(|e| panic!("article failed to decrypt: {e}"));
    decoded.name
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_article_mode_repost_keeps_restored_yenc_name_stable() {
    // PESTO-FINAL-2: under `--obfuscate article`, `repost_one` used to
    // generate a FRESH obfuscated yEnc name for each repost while re-encrypting
    // with the same salt/index. The yEnc `name=` is an AAD input (Body Std
    // v1.2 §2.1), so the reposted article's ciphertext differed from the
    // original — and an original/repost pair restored different names
    // despite identical salt/index bootstrap. Body Std v1.2 §8 requires
    // control-line/AAD values to stay stable across retry/repost.
    //
    // The mock misses the first two STATs (one check repost + one
    // recovery-pass repost, same recipe as
    // `encrypted_check_repost_and_recovery_stay_encrypted_with_same_identity`).
    let (addr, articles, _stat_count) = spawn_mock_server_with_stat_misses(2);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("article-mode.bin");
    let plaintext: Vec<u8> = (0..64u32).map(|i| (i * 7) as u8).collect();
    std::fs::write(&input, &plaintext).unwrap();

    let kdf_input = fixture_kdf_input();
    let cfg = config_with_obfuscation(addr, Some(kdf_input.into()), ObfuscateMode::Article);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files_with_progress(&cfg, &inputs, None, None, None)
        .await
        .unwrap();
    assert!(outcome.still_missing.is_empty());

    let captured = articles.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        3,
        "expected the original POST plus 2 reposts"
    );

    // Same salt/index bootstrap on every copy (stable segment identity).
    let indices = assert_all_articles_decrypt_to(&captured, kdf_input, &plaintext);
    assert_eq!(indices, vec![1u32; 3]);

    // The restored, AEAD-authenticated yEnc name must be IDENTICAL across
    // the original post, the STAT repost, and the recovery repost. A fresh
    // `obfuscated_name()` per repost breaks this.
    let names: Vec<String> = captured
        .iter()
        .map(|article| restored_yenc_name(article, kdf_input))
        .collect();
    let expected = names[0].clone();
    assert!(
        !expected.is_empty(),
        "restored yEnc name must be present in every article"
    );
    assert_eq!(
        names,
        vec![expected.clone(); 3],
        "restored yEnc name must be identical across original post, \
         STAT repost and recovery repost (Body Std v1.2 §8): {names:?}"
    );

    // The encrypted wire bodies must stay byte-identical for unchanged
    // input: same salt/index + same AAD name ⇒ deterministic re-encryption.
    let bodies: Vec<&[u8]> = captured.iter().map(|a| article_body(a)).collect();
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "encrypted article bodies must be byte-identical across \
         original post and reposts: the yEnc name is an AAD input and \
         must not change between attempts"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_article_mode_repost_still_regenerates_yenc_name() {
    // Control for the fix above: WITHOUT an encryption adapter, the
    // Article-mode repost arm keeps its upstream behavior of generating a
    // fresh obfuscated yEnc name per attempt (it is not authenticated
    // there, and rotating it is intentional upstream behavior guarded by
    // `check_repost_preserves_obfuscation.rs`).
    let (addr, articles, _stat_count) = spawn_mock_server_with_stat_misses(2);

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("plain-article-mode.bin");
    std::fs::write(&input, vec![7u8; 50_000]).unwrap();

    let cfg = config_with_obfuscation(addr, None, ObfuscateMode::Article);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files_with_progress(&cfg, &inputs, None, None, None)
        .await
        .unwrap();
    assert!(outcome.still_missing.is_empty());

    let captured = articles.lock().unwrap().clone();
    assert_eq!(
        captured.len(),
        3,
        "expected the original POST plus 2 reposts"
    );

    let names: Vec<String> = captured
        .iter()
        .map(|article| {
            let body = article_body(article);
            let text = String::from_utf8_lossy(body);
            text.lines()
                .find(|line| line.starts_with("=ybegin "))
                .and_then(|line| line.rsplit_once(" name="))
                .map(|(_, name)| name.trim_end_matches('\r').to_string())
                .expect("plain yEnc article must have an =ybegin name=")
        })
        .collect();
    let unique: std::collections::HashSet<_> = names.iter().collect();
    assert_eq!(
        unique.len(),
        names.len(),
        "ordinary Article-mode reposts must keep rotating the yEnc name \
         (upstream behavior unchanged): {names:?}"
    );
}
