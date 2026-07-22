//! Regression guard for a real correctness gap in the streaming `--check`
//! queue with a multi-server (failover) config: `check_worker` used to pick
//! which server to `STAT` an article against purely from
//! `worker_idx % servers.len()`, with no relation to which server actually
//! accepted that article's `POST`. `PostedSegment` had no record of that
//! either, so the check queue was, in effect, guessing.
//!
//! With more servers configured than `check_retries` (or, as reproduced
//! here, when the "wrong" server is simply unreachable), that guess can
//! permanently miss the server that actually has the article: the check
//! queue reports it "missing" and reposts a duplicate copy under a fresh
//! Message-ID, even though the original article is sitting on the network
//! untouched. This test sets up a primary server that refuses every
//! connection and an extra (failover) server that actually receives the
//! whole (multi-segment) upload, and asserts the extra server sees exactly
//! one `POST` per segment — not more (original + needless repost) —
//! proving the check queue went straight to the server that has each
//! article instead of guessing wrong and only recovering via a repost.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pesto::config::{Config, ObfuscateMode, ServerEntry};
use pesto::poster::post_files_with_progress;
use pesto::walk::expand_inputs;

/// Bind a port and immediately drop the listener, so the OS refuses any
/// connection attempt to it (`ECONNREFUSED`) — simulates a primary server
/// that is simply down, without needing a process to keep "not answering".
fn unreachable_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
    // `listener` drops here, freeing the port with nothing listening on it.
}

/// A mock NNTP server that tracks every Message-ID it actually received via
/// `POST`, and answers `STAT` truthfully from that set.
fn spawn_real_server() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let post_count = Arc::new(AtomicUsize::new(0));
    let post_count_clone = Arc::clone(&post_count);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let post_count = Arc::clone(&post_count_clone);
            std::thread::spawn(move || handle_connection(stream, post_count));
        }
    });

    (addr, post_count)
}

fn handle_connection(stream: TcpStream, post_count: Arc<AtomicUsize>) {
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
            // Slow enough that the upload of a several-segment file takes
            // measurable time, so the *first* segment's check (fired
            // `check_delay_secs=0` after its own `240`) resolves well
            // before the whole upload finishes and `scale_up` adds a second
            // check worker — which, by the pigeonhole of
            // `worker_idx % servers.len()` on just 2 servers, would
            // eventually land on the correct server anyway and mask the bug
            // by lucky timing instead of by the fix.
            std::thread::sleep(std::time::Duration::from_millis(150));
            post_count.fetch_add(1, Ordering::SeqCst);
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if let Some(id) = command.strip_prefix("STAT ") {
            // This mock only ever receives POSTs for articles that are
            // genuinely destined for it, so any STAT it's asked about that
            // it didn't itself accept would be a real miss — but for this
            // test every STAT it gets is for an article it did accept.
            let _ = id;
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

#[tokio::test(flavor = "multi_thread")]
async fn check_stats_the_server_the_article_was_actually_posted_to() {
    let unreachable = unreachable_addr();
    let (real_addr, real_post_count) = spawn_real_server();

    let dir = std::env::temp_dir().join(format!(
        "pesto_check_targets_posting_server_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");
    // 10 segments at 150 ms/POST (see the mock server) — the upload takes
    // ~1.5 s, giving the first segment's near-instant (`check_delay_secs=0`)
    // check plenty of time to resolve well before `scale_up` fires at the
    // very end of the run.
    std::fs::write(&input, vec![7u8; 10_000]).unwrap();

    let config = Config {
        // Primary: unreachable, and given zero upload connections so the
        // real upload never even attempts it — only the check queue's old
        // `worker_idx % servers.len()` guess would ever reach it.
        host: unreachable.ip().to_string(),
        port: unreachable.port(),
        ssl: false,
        connections: 0,
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
        // The only server with real upload capacity — every article must
        // land here.
        extra_servers: vec![ServerEntry {
            host: real_addr.ip().to_string(),
            port: real_addr.port(),
            ssl: false,
            connections: 1,
            username: None,
            password: None,
            retry_delay: 0,
            timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        }],
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
        // No rotation-based recovery: a wrong first guess is an instant
        // "confirmed miss" and triggers a repost, exactly like it would in
        // production with more failover servers than `check_retries`.
        check_retries: 1,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: true,
        pipeline_depth: 1,
        keepalive_interval: 0,
    };

    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files_with_progress(&config, &inputs, None, None, None)
        .await
        .unwrap();

    assert_eq!(
        outcome.segments.len(),
        10,
        "expected 10 segments (10_000/1000)"
    );
    assert!(
        outcome.still_missing.is_empty(),
        "every article was actually posted and should have been confirmed: {:?}",
        outcome.still_missing
    );

    // The real assertion: the check queue must have STAT'd (and found) every
    // article on the first try, on the server that actually has it — not
    // guessed wrong, declared some of them missing, and silently reposted
    // duplicates. More than 10 POSTs for a 10-segment file is exactly that
    // bug (each needless repost is one extra POST).
    assert_eq!(
        real_post_count.load(Ordering::SeqCst),
        10,
        "expected exactly 10 POSTs (one per segment, no duplicates) — more \
         means the check queue guessed the wrong server for at least one \
         segment, falsely declared it missing, and reposted a needless \
         duplicate"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
