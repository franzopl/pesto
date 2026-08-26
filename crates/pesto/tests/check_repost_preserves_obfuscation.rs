//! Regression guard: a `--check` repost of a missing article must reuse the
//! *obfuscated* wire identity the original post used, not the real
//! filename. `PostedSegment::subject_name` is always the real filename (for
//! NZB purposes — see `nzb.rs`'s `generate` doc comment), so before
//! `PostedSegment::wire_name` existed, `poster::check::repost_one` built the
//! repost's `Subject:`/yEnc `name=` straight from `subject_name` — silently
//! undoing `--obfuscate` for any article unlucky enough to need a repost.
//!
//! This posts one obfuscated single-segment file against a mock server that
//! fails the first two `STAT`s (forcing a normal repost and then the
//! automatic recovery-pass repost), and inspects the raw bytes of all 3
//! `POST`s received: none may contain the real filename anywhere.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files_with_progress;
use pesto::walk::expand_inputs;

const REAL_NAME: &str = "top-secret-movie.bin";

type CapturedArticles = Arc<Mutex<Vec<Vec<u8>>>>;

/// Mirrors `check_recover_pass.rs`'s proven mock: the first two `STAT`
/// replies report the article missing (triggering the normal
/// `check_post_retries` repost, then the automatic recovery-pass repost),
/// the third and any later `STAT` succeed. That reliably yields exactly 3
/// `POST`s (original + 2 reposts) for one single-segment upload.
fn spawn_mock_server() -> (SocketAddr, Arc<AtomicUsize>, CapturedArticles) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let stat_count = Arc::new(AtomicUsize::new(0));
    let articles = Arc::new(Mutex::new(Vec::new()));

    let stat_count_clone = Arc::clone(&stat_count);
    let articles_clone = Arc::clone(&articles);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let stat_count = Arc::clone(&stat_count_clone);
            let articles = Arc::clone(&articles_clone);
            std::thread::spawn(move || handle_connection(stream, stat_count, articles));
        }
    });

    (addr, stat_count, articles)
}

fn handle_connection(stream: TcpStream, stat_count: Arc<AtomicUsize>, articles: CapturedArticles) {
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
                article.extend_from_slice(&raw);
            }
            articles.lock().unwrap().push(article);
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

fn config(addr: SocketAddr) -> Config {
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
        obfuscate: ObfuscateMode::Full,
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
        // round, same as check_recover_pass.rs: the normal cycle gets 2
        // chances (original + 1 repost), both fail, then the automatic
        // recovery pass gets a 3rd.
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
async fn check_repost_of_a_missing_article_stays_obfuscated() {
    let (addr, _stat_count, articles) = spawn_mock_server();

    let dir = std::env::temp_dir().join(format!(
        "pesto_check_repost_obfuscation_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join(REAL_NAME);
    std::fs::write(&input, vec![7u8; 50_000]).unwrap();

    let cfg = config(addr);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files_with_progress(&cfg, &inputs, None, None, None)
        .await
        .unwrap();

    assert!(
        outcome.still_missing.is_empty(),
        "the recovery pass should have resolved the stubborn miss: {:?}",
        outcome.still_missing
    );

    let articles = articles.lock().unwrap();
    assert_eq!(
        articles.len(),
        3,
        "expected exactly 3 POSTs: the original plus 2 reposts (normal + recovery pass)"
    );

    for (i, article) in articles.iter().enumerate() {
        let text = String::from_utf8_lossy(article);
        assert!(
            !text.contains(REAL_NAME),
            "article #{i} must not contain the real filename anywhere \
             under --obfuscate=full:\n{text}"
        );
    }
}
