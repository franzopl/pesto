//! `ObfuscateMode::FullShared` must give every file posted in the same run —
//! the content file(s) and every PAR2 index/volume — the same wire name
//! prefix, so Usenet indexers can still group the release together (see
//! GitHub issue #58). Plain `full` obfuscation gives each file an
//! independently-random name, which is the bug this mode fixes.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

/// A config that processes files without touching the network (`dry_run`),
/// with the given obfuscation mode and no PAR2.
fn dry_run_config(obfuscate: ObfuscateMode) -> Config {
    Config {
        host: "unused".to_string(),
        port: 563,
        ssl: false,
        connections: 4,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 65536,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate,
        dry_run: true,
        par2: 0,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
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

/// Build a two-root directory tree under a fresh temp directory and return
/// `(temp_root, [directory args], [expected relative paths])`.
fn build_tree(tag: &str) -> (std::path::PathBuf, Vec<std::path::PathBuf>, Vec<String>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "pesto_full_shared_{tag}_{}_{}",
        std::process::id(),
        id
    ));
    let _ = std::fs::remove_dir_all(&root);

    let rels = [
        "ShowA/s01/ep01.bin",
        "ShowA/s01/ep02.bin",
        "ShowA/extras/clip.bin",
        "ShowB/movie.bin",
    ];
    for rel in rels {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0x5Au8; 100_000]).unwrap();
    }

    let args = vec![root.join("ShowA"), root.join("ShowB")];
    let expected = rels.iter().map(|r| r.to_string()).collect();
    (root, args, expected)
}

/// The shared-prefix portion of a `full-shared` wire name: everything before
/// the first `-` (multi-file suffix) or `.` (extension/PAR2 suffix).
fn shared_prefix(subject: &str) -> &str {
    let cut = subject.find(['-', '.']).unwrap_or(subject.len());
    &subject[..cut]
}

#[tokio::test]
async fn full_shared_obfuscation_uses_one_prefix_across_all_files() {
    let (root, args, expected) = build_tree("multi");

    let config = dry_run_config(ObfuscateMode::FullShared);
    let inputs = expand_inputs(&args).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    // Every file's subject must share the exact same prefix.
    let prefixes: std::collections::HashSet<&str> = outcome
        .segments
        .iter()
        .map(|s| shared_prefix(&s.subject_name))
        .collect();
    assert_eq!(
        prefixes.len(),
        1,
        "expected one shared prefix across the whole release, got: {prefixes:?}"
    );
    let prefix = *prefixes.iter().next().unwrap();
    assert!(
        (10..=30).contains(&prefix.len()) && prefix.chars().all(|c| c.is_ascii_alphanumeric()),
        "shared prefix `{prefix}` doesn't look like an obfuscated name"
    );

    // Distinct files must still get distinct (suffixed) subject names.
    let mut subjects: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.subject_name.as_str())
        .collect();
    subjects.sort_unstable();
    subjects.dedup();
    assert_eq!(subjects.len(), expected.len(), "subject names collided");

    // The real extension is preserved on the wire (unlike plain `full`, which
    // hides it), and the real path never leaks.
    for seg in &outcome.segments {
        assert!(
            seg.subject_name.ends_with(".bin"),
            "subject `{}` should keep the real extension",
            seg.subject_name
        );
    }
    assert!(
        !outcome
            .segments
            .iter()
            .any(|s| s.subject_name.contains("Show")),
        "a real path leaked into a subject"
    );

    // The NZB still carries the real relative paths.
    let nzb = pesto::nzb::generate(
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
    );
    for rel in &expected {
        assert!(
            nzb.contains(&format!("name=\"{rel}\"")),
            "real path `{rel}` missing from nzb name= attribute"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn full_shared_obfuscation_single_file_has_no_suffix() {
    let root =
        std::env::temp_dir().join(format!("pesto_full_shared_single_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("movie.mkv");
    std::fs::write(&file, vec![0x5Au8; 10_000]).unwrap();

    let config = dry_run_config(ObfuscateMode::FullShared);
    let inputs = expand_inputs(std::slice::from_ref(&file)).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();

    // A single-file release keeps a bare `prefix.ext`, with no `-NN` suffix.
    let subject = &outcome.segments[0].subject_name;
    assert!(
        subject.ends_with(".mkv") && !subject.contains('-'),
        "single-file subject `{subject}` should be a bare `prefix.mkv`"
    );

    std::fs::remove_dir_all(&root).ok();
}

// ── Real post with PAR2 enabled: the archive and its PAR2 set must share ──
// ── the same wire prefix, which is the exact grouping problem issue #58 ──
// ── reports against plain `full`. ──────────────────────────────────────────

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
async fn full_shared_obfuscation_par2_set_shares_prefix_with_content() {
    let addr = spawn_accept_all_server();
    let dir = std::env::temp_dir().join(format!("pesto_full_shared_par2_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");

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
        obfuscate: ObfuscateMode::FullShared,
        dry_run: false,
        par2: 10,
        par2_slice_size: Some(ARTICLE_SIZE),
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
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
    };

    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    let par2_segments: Vec<_> = outcome
        .segments
        .iter()
        .filter(|s| s.file_name.ends_with(".par2"))
        .collect();
    assert!(
        !par2_segments.is_empty(),
        "expected at least one PAR2 segment among: {:?}",
        outcome
            .segments
            .iter()
            .map(|s| &s.file_name)
            .collect::<Vec<_>>()
    );

    let content_segment = outcome
        .segments
        .iter()
        .find(|s| s.file_name == "movie.bin")
        .expect("no segment for movie.bin");

    // The whole point of `full-shared`: the content file and every PAR2 file
    // (index and volumes alike) must land on the wire under the same prefix,
    // unlike plain `full` where each gets an unrelated random name.
    let content_prefix = shared_prefix(&content_segment.subject_name);
    for seg in &par2_segments {
        assert_eq!(
            shared_prefix(&seg.subject_name),
            content_prefix,
            "PAR2 file `{}` subject `{}` doesn't share the release prefix",
            seg.file_name,
            seg.subject_name
        );
    }

    let _ = std::fs::remove_dir_all(&outcome.par2_temp_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
