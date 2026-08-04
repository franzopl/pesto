//! `--file-counter` (`Config::file_counter`) prefixes every subject with a
//! release-wide `[filenum/total]` counter — distinct from the per-file
//! segment counter `(part/total)` pesto always emits. `total` must count
//! every file in the release: data files, the PAR2 index, and every PAR2
//! volume `parmesan::layout::plan_volumes` produces — computed up front from
//! file sizes alone, before PAR2 encoding actually runs (see ROADMAP.md
//! "Subject file counter"). This is unrelated to GitHub issue #68 (closed as
//! indexer-side): enabling this flag does not change the `.volNNN+MMM.par2`
//! filename pattern indexers key their grouping on.

use pesto::config::{Config, ObfuscateMode};
use pesto::par2::layout::plan_volumes;
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

fn dry_run_config(file_counter: bool, par2_recovery_count: Option<usize>) -> Config {
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
        obfuscate: ObfuscateMode::None,
        dry_run: true,
        par2: 10,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count,
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
        compress_volume_size: None,
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
        file_counter,
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

fn build_two_files(tag: &str) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "pesto_file_counter_{tag}_{}_{}",
        std::process::id(),
        id
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let a = root.join("a.bin");
    let b = root.join("b.bin");
    std::fs::write(&a, vec![0x5Au8; 50_000]).unwrap();
    std::fs::write(&b, vec![0xA5u8; 50_000]).unwrap();
    (root, vec![a, b])
}

#[tokio::test(flavor = "multi_thread")]
async fn file_counter_off_emits_no_prefix() {
    let (root, files) = build_two_files("off");
    let config = dry_run_config(false, Some(7));
    let inputs = expand_inputs(&files).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();

    for seg in &outcome.segments {
        assert_eq!(seg.total_files, 0);
    }
    let xml = pesto::nzb::generate(
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
    );
    assert!(
        !xml.contains('['),
        "no file counter should appear when the flag is off"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn file_counter_numbers_data_files_index_and_every_volume() {
    let (root, files) = build_two_files("on");
    // Fixing the recovery-block count makes the expected volume layout
    // deterministic: plan_volumes(7) == [(0,1), (1,2), (3,4)] — 3 volumes.
    let recovery_count = 7u32;
    let expected_volumes = plan_volumes(recovery_count).len();
    let config = dry_run_config(true, Some(recovery_count as usize));
    let inputs = expand_inputs(&files).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    // 2 data files + 1 PAR2 index + `expected_volumes` volumes.
    let expected_total = (2 + 1 + expected_volumes) as u32;

    let mut seen_indices = std::collections::HashSet::new();
    for seg in &outcome.segments {
        assert_eq!(
            seg.total_files, expected_total,
            "every segment in the release must agree on the same total"
        );
        assert!(
            seg.file_index >= 1 && seg.file_index <= expected_total,
            "file_index {} out of range 1..={expected_total}",
            seg.file_index
        );
        seen_indices.insert(seg.file_index);
    }
    let expected_indices: std::collections::HashSet<u32> = (1..=expected_total).collect();
    assert_eq!(
        seen_indices, expected_indices,
        "file_index values must be a bijection onto 1..=total, covering data files, \
         the PAR2 index and every volume with no gaps or duplicates"
    );

    let xml = pesto::nzb::generate(
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
    );
    for i in 1..=expected_total {
        assert!(
            xml.contains(&format!("[{i}/{expected_total}]")),
            "nzb subject missing file counter [{i}/{expected_total}]:\n{xml}"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}
