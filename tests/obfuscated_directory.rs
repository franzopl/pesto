//! Phase 9c: an obfuscated directory upload must randomise every article's
//! subject and yEnc name across the whole tree, while the real relative paths
//! survive in the `.nzb` so a downloader can rebuild the directory layout.

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
        obfuscate,
        dry_run: true,
        par2: 0,
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
        indexer_url: None,
        indexer_api_key: None,
        indexer_category: None,
        no_upload: false,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: true,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        message_id_domain: None,
        post_hook: None,
        no_hooks: false,
        nfo: false,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 30,
        check_retries: 2,
    }
}

/// `true` when `s` is a 32-character lowercase-hex obfuscated name.
fn is_obfuscated_name(s: &str) -> bool {
    s.len() == 32
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Build a two-root directory tree under a fresh temp directory and return
/// `(temp_root, [directory args], [expected relative paths])`.
fn build_tree() -> (std::path::PathBuf, Vec<std::path::PathBuf>, Vec<String>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("pesto_obf_dir_{}_{}", std::process::id(), id));
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
        std::fs::write(&path, vec![0x5Au8; 1000]).unwrap();
    }

    let args = vec![root.join("ShowA"), root.join("ShowB")];
    let expected = rels.iter().map(|r| r.to_string()).collect();
    (root, args, expected)
}

#[tokio::test]
async fn full_obfuscation_randomises_subjects_but_keeps_paths_in_nzb() {
    let (root, args, expected) = build_tree();

    let config = dry_run_config(ObfuscateMode::Full);
    let inputs = expand_inputs(&args).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    // Every expected relative path must appear as a posted segment's real
    // file name, and its subject must be a fresh obfuscated name — never the
    // path itself.
    for rel in &expected {
        let seg = outcome
            .segments
            .iter()
            .find(|s| &s.file_name == rel)
            .unwrap_or_else(|| panic!("no segment for `{rel}`"));
        assert!(
            is_obfuscated_name(&seg.subject_name),
            "subject `{}` for `{rel}` is not obfuscated",
            seg.subject_name
        );
    }

    // Distinct files get distinct obfuscated names across the whole tree.
    let mut subjects: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.subject_name.as_str())
        .collect();
    subjects.sort_unstable();
    subjects.dedup();
    assert_eq!(subjects.len(), expected.len(), "obfuscated names collided");

    // With obfuscate=full the `name=` attribute must also be the randomised
    // token, not the real path — nothing in the .nzb reveals the original name.
    let nzb = pesto::nzb::generate(
        &config.from,
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
        true,
    );
    for rel in &expected {
        assert!(
            !nzb.contains(&format!("name=\"{rel}\"")),
            "real path `{rel}` leaked into nzb name= attribute"
        );
    }
    assert!(
        !nzb.contains("subject=\"Show") && !nzb.contains("name=\"Show"),
        "a real path leaked into the nzb"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn subject_obfuscation_keeps_relative_path_as_yenc_name() {
    let (root, args, expected) = build_tree();

    // `subject` mode obfuscates only the subject; the yEnc name — and so the
    // segment's published name — stays the real relative path.
    let config = dry_run_config(ObfuscateMode::Subject);
    let inputs = expand_inputs(&args).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    for rel in &expected {
        let seg = outcome
            .segments
            .iter()
            .find(|s| &s.file_name == rel)
            .unwrap_or_else(|| panic!("no segment for `{rel}`"));
        assert!(is_obfuscated_name(&seg.subject_name));
    }

    std::fs::remove_dir_all(&root).ok();
}
