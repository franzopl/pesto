//! Regression guard for a real gap found by comparing `pesto`'s yEnc article
//! construction against `nyuu`'s: the whole-file CRC-32 (`crc32=` on the
//! `=yend` line) was never included on a multi-part file's *last* segment,
//! only the per-part `pcrc32=`. `nyuu` has always included it (see
//! `MultiEncoder` in `lib/article.js`: `if(this.part == this.parts) fullCrc
//! += ' crc32='+this.crc.toString('hex');`).
//!
//! This doesn't explain the missing-article investigation (a file posts at
//! most a handful of "last segments" — far fewer than the failures observed
//! across many uploads — so it was ruled out as the root cause), but it's a
//! real yEnc spec-completeness gap worth closing regardless.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

/// A mock NNTP server that accepts every `POST` with `240` and records the
/// full raw article body (headers + yEnc data) it received, keyed by the
/// part number parsed from the `=ybegin`/`=ypart` line.
fn spawn_capturing_server() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let captured = Arc::clone(&captured_clone);
            std::thread::spawn(move || handle_connection(stream, captured));
        }
    });

    (addr, captured)
}

fn handle_connection(stream: TcpStream, captured: Arc<Mutex<Vec<String>>>) {
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
                article.extend_from_slice(&raw);
            }
            captured
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&article).into_owned());
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
    (0..500_000u64)
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
async fn last_segment_of_a_multipart_file_carries_the_whole_file_crc32() {
    let (addr, captured) = spawn_capturing_server();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    // article_size below forces 5 parts (500_000 / 100_000).
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
        article_size: 100_000,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
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
    assert_eq!(
        outcome.segments.len(),
        5,
        "expected 5 parts (500_000/100_000)"
    );

    let articles = captured.lock().unwrap();
    assert_eq!(articles.len(), 5);

    let with_full_crc = articles.iter().filter(|a| a.contains(" crc32=")).count();
    assert_eq!(
        with_full_crc, 1,
        "exactly one segment (the last) should carry the whole-file crc32:\n{articles:#?}"
    );

    let last = articles
        .iter()
        .find(|a| a.contains("=ypart begin=400001 end=500000"))
        .expect("expected to find the last part (bytes 400001..500000)");
    assert!(
        last.contains(" crc32="),
        "the last segment must carry the whole-file crc32:\n{last}"
    );
    assert!(
        last.contains(" pcrc32="),
        "the last segment must still carry its own per-part crc32 too:\n{last}"
    );

    let first = articles
        .iter()
        .find(|a| a.contains("=ypart begin=1 end=100000"))
        .expect("expected to find the first part");
    assert!(
        !first.contains(" crc32="),
        "a non-last segment must not carry the whole-file crc32:\n{first}"
    );
}
