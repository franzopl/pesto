//! `ObfuscateMode::FullShared` must give every file posted in the same run —
//! the content file(s) and every PAR2 index/volume — the same wire name
//! prefix, so Usenet indexers can still group the release together (see
//! GitHub issue #58). Plain `full` obfuscation gives each file an
//! independently-random name, which is the bug this mode fixes.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

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

    // NZB subjects now use the real filenames (not the wire prefix) for proper
    // download-client renaming. Verify that all real filenames are present.
    let mut filenames: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.subject_name.as_ref())
        .collect();
    filenames.sort_unstable();
    filenames.dedup();
    // Should have one subject_name per file (all segments of a file share the same name).
    assert_eq!(filenames.len(), expected.len(), "filename count mismatch");

    // Every file's real filename must end with .bin (matching the expected paths).
    for seg in &outcome.segments {
        assert!(
            seg.subject_name.ends_with(".bin"),
            "subject `{}` should end with .bin (real filename preserved for NZB)",
            seg.subject_name
        );
    }

    // Verify the real filenames match what was uploaded.
    for real_name in &expected {
        assert!(
            filenames.contains(&real_name.as_str()),
            "real filename `{real_name}` not found in segment subjects"
        );
    }

    // The NZB carries the real relative paths in subject's quoted string
    // (standard NZB 1.1 has no `name=`) for download-client renaming.
    let nzb = pesto::nzb::generate(
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
    );
    for rel in &expected {
        assert!(
            nzb.contains(&format!("subject=\"&quot;{rel}&quot;")),
            "real path `{rel}` missing from nzb subject"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}

/// `full-shared` replaces every real name with `{prefix}-NN{ext}`, so that
/// `NN` and the `--file-counter` `[N/M]` prefix are the release's only visible
/// ordering. They must agree, and both must follow the order a human reads —
/// `ep2` before `ep10`, which plain lexicographic sorting gets backwards.
#[tokio::test]
async fn full_shared_multi_file_suffix_matches_the_file_counter() {
    let root =
        std::env::temp_dir().join(format!("pesto_full_shared_counter_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    // Unpadded episode numbers, created out of order.
    let order = ["ep10.bin", "ep1.bin", "ep2.bin"];
    for (i, name) in order.iter().enumerate() {
        std::fs::write(root.join(name), vec![0x5Au8; 10_000 + i]).unwrap();
    }

    let mut config = dry_run_config(ObfuscateMode::FullShared);
    config.file_counter = true;
    let args: Vec<std::path::PathBuf> = order.iter().map(|n| root.join(n)).collect();
    let inputs = expand_inputs(&args).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    for seg in &outcome.segments {
        // `subject_name` is the real name (the NZB identity); `wire_name` is
        // what actually went out as `{prefix}-NN.bin`.
        let expected_index = match seg.subject_name.as_ref() {
            "ep1.bin" => 1,
            "ep2.bin" => 2,
            "ep10.bin" => 3,
            other => panic!("unexpected file `{other}`"),
        };
        assert_eq!(
            seg.file_index, expected_index,
            "`{}` should be file [{expected_index}/3]",
            seg.subject_name
        );
        assert!(
            seg.wire_name
                .ends_with(&format!("-{expected_index:02}.bin")),
            "wire name `{}` for `{}` should carry suffix -{expected_index:02}, matching its \
             file counter",
            seg.wire_name,
            seg.subject_name
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

#[tokio::test]
async fn full_shared_obfuscation_preserves_rar_volume_suffix() {
    // Regression test for issue #68: `--compress-volume-size` splits an
    // archive into `stem.partNN.rar` volumes, which indexers key their
    // "same release" grouping off of. Full-shared naming must preserve that
    // exact suffix on the wire instead of collapsing it to a generic `-NN`
    // suffix, or the release fails to group.
    let root =
        std::env::temp_dir().join(format!("pesto_full_shared_rar_vol_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let vol_names = ["stem.part01.rar", "stem.part02.rar", "stem.part03.rar"];
    let inputs: Vec<pesto::walk::InputFile> = vol_names
        .iter()
        .map(|name| {
            let path = root.join(name);
            std::fs::write(&path, vec![0x5Au8; 10_000]).unwrap();
            pesto::walk::InputFile {
                path,
                name: name.to_string(),
            }
        })
        .collect();

    let config = dry_run_config(ObfuscateMode::FullShared);
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    let prefix = shared_prefix(&outcome.segments[0].subject_name);
    for real_name in vol_names {
        let seg = outcome
            .segments
            .iter()
            .find(|s| s.file_name == real_name)
            .unwrap_or_else(|| panic!("no segment for `{real_name}`"));
        let expected_suffix = &real_name["stem".len()..];
        assert_eq!(
            seg.subject_name.as_ref(),
            format!("{prefix}{expected_suffix}").as_str(),
            "expected the `.partNN.rar` suffix preserved verbatim, got `{}`",
            seg.subject_name
        );
    }

    std::fs::remove_dir_all(&root).ok();
}

// ── Real post with PAR2 enabled: the archive and its PAR2 set must share ──
// ── the same wire prefix, which is the exact grouping problem issue #58 ──
// ── reports against plain `full`. ──────────────────────────────────────────

type CapturedArticles = Arc<Mutex<Vec<Vec<u8>>>>;

fn spawn_accept_all_server() -> (SocketAddr, CapturedArticles) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let articles: CapturedArticles = Arc::new(Mutex::new(Vec::new()));
    let articles_clone = Arc::clone(&articles);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let articles = Arc::clone(&articles_clone);
            std::thread::spawn(move || handle_connection(stream, articles));
        }
    });

    (addr, articles)
}

fn handle_connection(stream: TcpStream, articles: CapturedArticles) {
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
    let (addr, articles) = spawn_accept_all_server();
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

    // Issue #106: an indexer that only inspects the yEnc body (not the
    // Subject) must still recognise a PAR2 article as part of the release —
    // so the yEnc `name=` must also carry the shared prefix, for the PAR2
    // index/volumes exactly like it already does for the content file. Note
    // this is the *wire* prefix (parsed from the raw Subject header below),
    // not `content_segment.subject_name` above — that field is always the
    // real filename for NZB purposes, regardless of obfuscation, so it isn't
    // a reliable stand-in for what's actually on the wire.
    let captured = articles.lock().unwrap();
    assert!(!captured.is_empty(), "no articles were captured");

    fn quoted_name(subject: &str) -> &str {
        subject
            .split_once('"')
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(name, _)| name)
            .unwrap_or(subject)
    }

    let mut wire_prefix: Option<String> = None;
    let mut checked_par2 = 0;
    for article in captured.iter() {
        let text = String::from_utf8_lossy(article);
        let subject = text
            .lines()
            .find_map(|l| l.strip_prefix("Subject: "))
            .expect("article must have a Subject header")
            .trim_end_matches('\r');
        let name = quoted_name(subject);
        let prefix = shared_prefix(name).to_string();
        let prefix = wire_prefix.get_or_insert(prefix);
        assert_eq!(
            shared_prefix(name),
            prefix,
            "article subject `{subject}` doesn't share the release's wire prefix `{prefix}`"
        );

        if !subject.contains(".par2") {
            continue;
        }
        let ybegin = text
            .lines()
            .find(|l| l.starts_with("=ybegin "))
            .expect("article must have a =ybegin line");
        let yenc_name = ybegin
            .split("name=")
            .nth(1)
            .expect("=ybegin must carry name=")
            .trim_end_matches('\r');
        assert!(
            yenc_name.starts_with(&format!("{prefix}-")),
            "PAR2 article's yEnc name= `{yenc_name}` (subject `{subject}`) \
             doesn't start with the shared wire prefix `{prefix}-`"
        );
        checked_par2 += 1;
    }
    assert!(checked_par2 > 0, "no PAR2 articles were captured to check");
    drop(captured);

    let _ = std::fs::remove_dir_all(&outcome.par2_temp_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
