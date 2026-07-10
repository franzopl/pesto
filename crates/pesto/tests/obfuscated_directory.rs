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
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
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
        check: false,
        check_delay_secs: 30,
        check_retries: 2,
        check_connections: 1,
        check_post_retries: 1,
        pipeline_depth: 1,
        keepalive_interval: 0,
    }
}

/// `true` when `s` looks like a pesto obfuscated name: 10–30 alphanumeric chars.
fn is_obfuscated_name(s: &str) -> bool {
    (10..=30).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric())
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
        std::fs::write(&path, vec![0x5Au8; 100_000]).unwrap();
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

    // The NZB always carries the real filename even in full obfuscation mode.
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
    assert!(
        !nzb.contains("subject=\"Show"),
        "a real path leaked into the nzb subject"
    );

    // Verify that each <file> element has a distinct poster (from) value.
    let mut posters: Vec<&str> = Vec::new();
    for line in nzb.lines() {
        if line.trim().starts_with("<file ") {
            if let Some(start) = line.find("poster=\"") {
                let start = start + 8;
                if let Some(end) = line[start..].find('"') {
                    posters.push(&line[start..start + end]);
                }
            }
        }
    }
    // Distinct files should have distinct posters in full obfuscation mode.
    let unique_posters: std::collections::HashSet<_> = posters.iter().copied().collect();
    assert_eq!(
        unique_posters.len(),
        posters.len(),
        "posters should be unique per file in full obfuscation mode"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn full_obfuscation_nzb_reflects_random_dates() {
    let (root, args, _expected) = build_tree();

    let mut config = dry_run_config(ObfuscateMode::Full);
    config.date = Some("random".into());
    let inputs = expand_inputs(&args).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();

    let nzb = pesto::nzb::generate(
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
    );

    // Collect all date="..." values from the NZB.
    let mut dates: Vec<u64> = Vec::new();
    for line in nzb.lines() {
        if line.trim().starts_with("<file ") {
            if let Some(start) = line.find("date=\"") {
                let start = start + 6;
                if let Some(end) = line[start..].find('"') {
                    let date_str = &line[start..start + end];
                    if let Ok(ts) = date_str.parse::<u64>() {
                        dates.push(ts);
                    }
                }
            }
        }
    }

    // Distinct files should have distinct dates when date = random.
    let unique_dates: std::collections::HashSet<_> = dates.iter().copied().collect();
    assert!(
        unique_dates.len() > 1 || dates.len() <= 1,
        "random dates should vary across files; got {dates:?}"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn full_obfuscation_same_file_same_date() {
    let (root, args, expected) = build_tree();

    let mut config = dry_run_config(ObfuscateMode::Full);
    config.date = Some("random".into());
    let inputs = expand_inputs(&args).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();

    // Every segment of the same file must share the exact same date.
    for rel in &expected {
        let segs: Vec<_> = outcome
            .segments
            .iter()
            .filter(|s| &s.file_name == rel)
            .collect();
        assert!(
            segs.len() > 1 || expected.len() == 1,
            "need multiple segments per file to test date sharing"
        );
        let first_date = segs[0].date.clone();
        for seg in &segs[1..] {
            assert_eq!(
                seg.date, first_date,
                "segments of `{rel}` have different dates: {:?} vs {:?}",
                first_date, seg.date
            );
        }
    }

    std::fs::remove_dir_all(&root).ok();
}
