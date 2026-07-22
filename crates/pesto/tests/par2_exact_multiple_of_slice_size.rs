//! Regression guard for a real crash: when a file's size is an exact
//! multiple of the PAR2 slice size, `producer`'s per-article accumulation
//! loop in `crates/pesto/src/poster/mod.rs` used to drain `par2_accum` to
//! exactly zero and flush the file's true last slice with
//! `is_last_of_file: false` (the trailing, correctly-labelled flush is
//! skipped whenever there's nothing left over). The PAR2 worker
//! (`crates/parmesan/src/worker.rs`) relies on that flag to finalize and
//! push the file's MD5/CRC hash — without it, the hash silently bled into
//! the next file, or (for the last file in the set) the worker returned
//! fewer hashes than non-empty files and `producer` panicked with
//! `"worker returned fewer hashes than non-empty files"`.
//!
//! This reproduces the exact condition (`article_size == par2_slice_size`,
//! file size an exact multiple of both) against the standard posting path
//! (not `--par2-only`, which was never affected — see the module comment on
//! `par2_only_ingest`).

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

fn spawn_accept_all_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || handle_connection(stream));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream) {
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
            if writer.write_all(b"240 article received\r\n").is_err() {
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

fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len as u64)
        .map(|i| {
            let mut z = i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (seed as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z >> 33) as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn file_size_exact_multiple_of_par2_slice_size_does_not_panic() {
    let addr = spawn_accept_all_server();
    let dir =
        std::env::temp_dir().join(format!("pesto_par2_exact_multiple_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");

    // article_size == par2_slice_size, and the file is exactly 10 articles —
    // every article boundary is also an exact PAR2 slice boundary, the
    // precise condition that used to drain `par2_accum` to zero. 10 slices
    // at 10% recovery rounds down to exactly 1 recovery block, so the PAR2
    // worker (and the buggy code path) is actually exercised — too few
    // slices makes `recovery_count` round down to 0 and skips PAR2 entirely.
    const ARTICLE_SIZE: usize = 65536;
    const ARTICLES: usize = 10;
    std::fs::write(&input, content(0, ARTICLE_SIZE * ARTICLES)).unwrap();

    let config = Config {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: ARTICLE_SIZE,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 10,
        par2_slice_size: Some(ARTICLE_SIZE),
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
        history: true,
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
        check: false,
        check_delay_secs: 5,
        check_retries: 2,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        pipeline_depth: 1,
        keepalive_interval: 0,
    };

    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    // The bug manifested as a panic (`.expect("worker returned fewer hashes
    // than non-empty files")`) inside `producer`, unwinding straight through
    // this `.await.unwrap()` — so simply completing without panicking is the
    // regression check.
    let outcome = post_files(&config, &inputs).await.unwrap();

    assert!(
        outcome
            .segments
            .iter()
            .any(|s| s.file_name.ends_with(".par2")),
        "expected at least one PAR2 segment among: {:?}",
        outcome
            .segments
            .iter()
            .map(|s| &s.file_name)
            .collect::<Vec<_>>()
    );

    let par2_dir = pesto::poster::par2_temp_dir();
    let _ = std::fs::remove_dir_all(&par2_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
