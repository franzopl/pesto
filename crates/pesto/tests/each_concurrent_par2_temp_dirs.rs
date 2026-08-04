//! Regression guard for GitHub issue #67: `--each`/`--season` with
//! `--jobs > 1` runs several `post_files_with_progress_and_cancel` calls
//! concurrently *in the same process*. `poster::par2_temp_dir()` used to be
//! keyed only by the process ID, so every concurrent entry wrote its PAR2
//! index/volume files into the exact same directory — and each entry's
//! caller deletes that directory (`remove_dir_all`) as soon as its own run
//! finishes, which could wipe out a sibling entry's still-in-flight PAR2
//! source files (needed later for `--check`'s repost pass).
//!
//! This runs two `post_files` calls concurrently and checks each gets its
//! own PAR2 temp directory (`PostOutcome::par2_temp_dir`), so finishing one
//! and cleaning it up can never touch the other's files.

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

fn par2_config(port: u16) -> Config {
    const ARTICLE_SIZE: usize = 65536;
    Config {
        host: "127.0.0.1".to_string(),
        port,
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
        par2_temp_dir: None,
        compress_temp_dir: None,
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
        check_delay_secs: 5,
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
async fn concurrent_each_entries_get_distinct_par2_temp_dirs() {
    let addr = spawn_accept_all_server();
    let dir =
        std::env::temp_dir().join(format!("pesto_each_concurrent_par2_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    const ARTICLE_SIZE: usize = 65536;
    const ARTICLES: usize = 10;

    let input_a = dir.join("entry_a").join("movie.bin");
    let input_b = dir.join("entry_b").join("movie.bin");
    std::fs::create_dir_all(input_a.parent().unwrap()).unwrap();
    std::fs::create_dir_all(input_b.parent().unwrap()).unwrap();
    std::fs::write(&input_a, content(1, ARTICLE_SIZE * ARTICLES)).unwrap();
    std::fs::write(&input_b, content(2, ARTICLE_SIZE * ARTICLES)).unwrap();

    let config_a = par2_config(addr.port());
    let config_b = par2_config(addr.port());
    let inputs_a = expand_inputs(std::slice::from_ref(&input_a)).unwrap();
    let inputs_b = expand_inputs(std::slice::from_ref(&input_b)).unwrap();

    // Simulate `--each --jobs 2`: two entries posted concurrently from the
    // same process, exactly like `run_batch` in `bin/pesto.rs` does.
    let (outcome_a, outcome_b) = tokio::join!(
        post_files(&config_a, &inputs_a),
        post_files(&config_b, &inputs_b),
    );
    let outcome_a = outcome_a.unwrap();
    let outcome_b = outcome_b.unwrap();

    assert!(
        outcome_a.failures.is_empty(),
        "entry A: {:?}",
        outcome_a.failures
    );
    assert!(
        outcome_b.failures.is_empty(),
        "entry B: {:?}",
        outcome_b.failures
    );

    // The actual fix: each concurrent run must get its own PAR2 temp dir.
    assert_ne!(
        outcome_a.par2_temp_dir, outcome_b.par2_temp_dir,
        "concurrent entries shared one PAR2 temp dir — deleting either \
         after it finishes would destroy the other's still-needed PAR2 files"
    );

    // Both directories must independently hold their own PAR2 output —
    // deleting one (as a finished entry's caller would) must not have
    // touched the other's files.
    for outcome in [&outcome_a, &outcome_b] {
        assert!(
            outcome.par2_temp_dir.exists(),
            "{} should still exist — no cleanup has run yet",
            outcome.par2_temp_dir.display()
        );
        let has_par2_file = std::fs::read_dir(&outcome.par2_temp_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|ext| ext == "par2"));
        assert!(
            has_par2_file,
            "{} should contain this entry's own PAR2 file(s)",
            outcome.par2_temp_dir.display()
        );
    }

    // Deleting entry A's directory (as its caller would once fully done)
    // must leave entry B's directory and files completely intact.
    std::fs::remove_dir_all(&outcome_a.par2_temp_dir).unwrap();
    assert!(
        outcome_b.par2_temp_dir.exists(),
        "cleaning up entry A's temp dir destroyed entry B's"
    );
    let b_still_has_par2 = std::fs::read_dir(&outcome_b.par2_temp_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.path().extension().is_some_and(|ext| ext == "par2"));
    assert!(
        b_still_has_par2,
        "entry B's PAR2 file(s) disappeared after cleaning up entry A"
    );

    let _ = std::fs::remove_dir_all(&outcome_b.par2_temp_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
