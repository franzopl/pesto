//! PAR2 recovery blocks are computed over a release's *entire* recovery set
//! together, not per file — so if a data file's content changes between
//! runs, every PAR2 volume's previously-recorded resume segments are just as
//! untrustworthy as that file's own, even though PAR2 volumes never go
//! through the per-file fingerprint check themselves (they're generated
//! straight into the posting queue — see `push_par2_file`). This is the
//! fix documented alongside issue #18's resume follow-up (task #9): a
//! per-file mismatch with `--par2` active must discard the *whole* resume
//! state, not just the changed file's own segments.
//!
//! `pesto::resume`'s own unit tests cover `forget_all_segments` in
//! isolation; this exercises the real wiring end to end through
//! `post_files_with_progress` with actual PAR2 generation and posting.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files_with_progress;
use pesto::resume::{FileFingerprint, ResumeState, RunFingerprint};

/// Accepts every `POST` unconditionally — PAR2 volumes and data segments
/// alike.
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

fn test_config(port: u16) -> Config {
    Config {
        host: "127.0.0.1".to_string(),
        port,
        ssl: false,
        connections: 2,
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 500,
        line_length: 128,
        retries: 3,
        retry_delay: 0,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        proxy: None,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 100,
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
        resume: true,
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

#[tokio::test(flavor = "multi_thread")]
async fn changed_file_with_par2_invalidates_previously_resumed_par2_volumes_too() {
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
    let path = dir.path().join("movie.bin");
    std::fs::write(&path, vec![0x42_u8; 2000]).unwrap();
    let state_path = dir.path().join("movie.bin.pesto-state");

    let config = test_config(addr.port());

    // Simulate a prior run over *different* content (old size), whose state
    // recorded both the data segment and a PAR2 volume — the PAR2 volume
    // entry is hand-crafted, since PAR2 volumes never go through the
    // per-file fingerprint check that would otherwise catch this on their
    // own (see the module doc comment).
    let mut prior = ResumeState::default();
    prior.validate_run(&RunFingerprint::from_config(&config));
    prior.record_file(
        "movie.bin",
        FileFingerprint {
            size: 999, // does not match the 2000-byte file actually on disk
            mtime: Some(1),
        },
    );
    prior.record("movie.bin", 1, "old-data-part1@prior-run.example", 500);
    prior.record(
        "movie.bin.vol000+001.par2",
        1,
        "old-par2-vol@prior-run.example",
        500,
    );
    prior.save(&state_path).unwrap();

    let inputs = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "movie.bin".to_string(),
    }];
    let outcome = post_files_with_progress(&config, &inputs, None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(outcome.failures.is_empty());
    // At least the 4 data segments (2000 / 500) plus some PAR2 volume(s).
    assert!(
        outcome.segments.len() > 4,
        "expected data segments plus at least one freshly generated PAR2 volume, got {}",
        outcome.segments.len()
    );

    let ids: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.message_id.as_str())
        .collect();
    assert!(
        !ids.contains(&"old-data-part1@prior-run.example"),
        "the changed file's own stale segment must not be reused"
    );
    assert!(
        !ids.contains(&"old-par2-vol@prior-run.example"),
        "a PAR2 volume's stale segment must not be reused either, even though PAR2 volumes \
         never go through the per-file fingerprint check on their own"
    );
    // Every segment was freshly (re-)posted: the whole recovery set had to
    // be regenerated once the underlying data was considered changed.
    assert_eq!(posts.load(Ordering::Relaxed), outcome.segments.len());
}
