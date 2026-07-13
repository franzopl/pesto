//! Regression guard for a real production bug: `post_files`/
//! `post_files_with_progress` used to delete the PAR2 temp directory
//! (`poster::par2_temp_dir()`) as soon as the main post loop finished.
//! `--check`'s repost pass and the end-of-run failed-task retry both run
//! *after* that point (from the callers in `bin/pesto.rs` / `upload.rs`),
//! and both re-read a segment's source file from disk to repost it — for a
//! PAR2-file segment, that source file only exists inside this temp
//! directory. Deleting it too early made any PAR2 segment that came up
//! missing during `--check` permanently unrepostable ("cannot open file: No
//! such file or directory"), no matter how many repost rounds were
//! configured. This test confirms the directory is still there right after
//! posting finishes, before any caller-side repost pass would run.

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

/// A trivial mock NNTP server that accepts every `POST` with a plain `240`.
/// No `STAT` support is needed — this test never checks or reposts.
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

fn content(seed: u8) -> Vec<u8> {
    (0..2_000_000u64)
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
async fn par2_temp_dir_is_not_deleted_by_post_files() {
    let addr = spawn_accept_all_server();
    let dir = std::env::temp_dir().join(format!("pesto_par2_survives_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");
    std::fs::write(&input, content(0)).unwrap();

    let config = Config {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 65536,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 10,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
        par2_only: false,
        threads: 0,
        simd: pesto::par2::SimdPath::Auto,
        extra_servers: vec![],
        verify: false,
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: vec![],
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
        check: true,
        check_delay_secs: 30,
        check_retries: 2,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        pipeline_depth: 1,
        keepalive_interval: 0,
    };

    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();

    // At least one posted segment must be a PAR2 file, or this test isn't
    // exercising anything.
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

    // The whole point: the temp dir holding those PAR2 files' source bytes
    // must still exist immediately after post_files returns, so a caller
    // running --check afterward can still re-read them to repost a segment
    // STAT couldn't find.
    let par2_dir = pesto::poster::par2_temp_dir();
    assert!(
        par2_dir.exists(),
        "par2_temp_dir() ({}) must still exist right after post_files() \
         returns — --check's repost pass runs after this and needs it",
        par2_dir.display()
    );

    let _ = std::fs::remove_dir_all(&par2_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
