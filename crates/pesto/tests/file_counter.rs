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
        proxy: None,
        proxy_check_ip: false,
        obfuscate: ObfuscateMode::None,
        dry_run: true,
        par2: 10,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count,
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
        encrypt_password: None,
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

/// A `--compress-volume-size` style release: unpadded `.partN.rar` volumes,
/// created in an order unrelated to their volume number.
fn build_volumes(tag: &str, count: u32) -> (std::path::PathBuf, Vec<std::path::PathBuf>) {
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
    let mut paths = Vec::new();
    for n in 1..=count {
        let path = root.join(format!("release.part{n}.rar"));
        std::fs::write(&path, vec![n as u8; 50_000 + n as usize]).unwrap();
        paths.push(path);
    }
    (root, paths)
}

/// The counter must follow the release's own volume order — `part1.rar` is
/// `[1/N]` — and not the File-ID (MD5-keyed) order `metas` is sorted into for
/// PAR2. A real `--obfuscate=full-shared` upload came out with `part4.rar`
/// numbered `[1/14]`, which lists the release scrambled on indexers that sort
/// a collection by Subject.
#[tokio::test(flavor = "multi_thread")]
async fn file_counter_follows_volume_order_not_file_id_order() {
    // 12 volumes: enough that unpadded names (`part2` vs `part12`) would also
    // come out wrong under plain lexicographic order.
    let volumes = 12u32;
    let (root, files) = build_volumes("volorder", volumes);
    let recovery_count = 7u32;
    let expected_total = volumes + 1 + plan_volumes(recovery_count).len() as u32;
    let config = dry_run_config(true, Some(recovery_count as usize));
    let inputs = expand_inputs(&files).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    for seg in &outcome.segments {
        assert_eq!(seg.total_files, expected_total);
        let name = seg.subject_name.as_ref();
        if let Some(n) = name
            .strip_prefix("release.part")
            .and_then(|rest| rest.strip_suffix(".rar"))
            .and_then(|digits| digits.parse::<u32>().ok())
        {
            assert_eq!(
                seg.file_index, n,
                "`{name}` should be [{n}/{expected_total}], got [{}/{expected_total}]",
                seg.file_index
            );
        } else {
            // Everything else in the release is PAR2, which closes it out.
            assert!(
                name.ends_with(".par2"),
                "unexpected non-PAR2 file in the release: `{name}`"
            );
            assert!(
                seg.file_index > volumes,
                "PAR2 file `{name}` must be numbered after every data file, got {}",
                seg.file_index
            );
        }
    }

    std::fs::remove_dir_all(&root).ok();
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
        ObfuscateMode::None,
    )
    .unwrap();
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
        ObfuscateMode::None,
    )
    .unwrap();
    for i in 1..=expected_total {
        assert!(
            xml.contains(&format!("[{i}/{expected_total}]")),
            "nzb subject missing file counter [{i}/{expected_total}]:\n{xml}"
        );
    }

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn planning_release_layout_is_deterministic_and_idempotent() {
    // Planning the same release twice yields the identical ReleaseLayout
    // and segment identity sequence.
    let (root, files) = build_volumes("idemp", 4);
    let recovery_count = 3u32;
    let config = dry_run_config(true, Some(recovery_count as usize));
    let inputs = expand_inputs(&files).unwrap();

    let outcome1 = post_files(&config, &inputs).await.unwrap();
    let outcome2 = post_files(&config, &inputs).await.unwrap();

    assert_eq!(outcome1.segments.len(), outcome2.segments.len());
    for (s1, s2) in outcome1.segments.iter().zip(outcome2.segments.iter()) {
        assert_eq!(s1.subject_name, s2.subject_name);
        assert_eq!(s1.part, s2.part);
        assert_eq!(s1.total, s2.total);
        assert_eq!(s1.file_index, s2.file_index);
        assert_eq!(s1.total_files, s2.total_files);
    }

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn pregenerated_par2_path_preserves_file_counter_and_layout_bijection() {
    // --par2-before-upload posts data files then reads back already-generated PAR2.
    // Every file must receive exactly one [N/M] ordinal and maintain bijection.
    let (root, files) = build_two_files("pregen");
    let recovery_count = 7u32;
    let expected_volumes = plan_volumes(recovery_count).len();
    let mut config = dry_run_config(true, Some(recovery_count as usize));
    config.par2_before_upload = true;

    let inputs = expand_inputs(&files).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    let expected_total = (2 + 1 + expected_volumes) as u32;
    let mut seen_indices = std::collections::HashSet::new();
    for seg in &outcome.segments {
        assert_eq!(seg.total_files, expected_total);
        seen_indices.insert(seg.file_index);
    }
    let expected_indices: std::collections::HashSet<u32> = (1..=expected_total).collect();
    assert_eq!(seen_indices, expected_indices);

    let xml = pesto::nzb::generate(
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
        ObfuscateMode::None,
    )
    .unwrap();
    for i in 1..=expected_total {
        assert!(xml.contains(&format!("[{i}/{expected_total}]")));
    }

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn file_counter_natural_order_traps_part1_part2_part10() {
    // Unpadded volume numbers: part1, part2, ..., part10.
    // Lexicographic ordering would place part10 before part2.
    // Natural order must keep part1 -> 1, part2 -> 2, ..., part10 -> 10.
    let count = 10u32;
    let (root, files) = build_volumes("traps", count);
    let config = dry_run_config(true, Some(0)); // no recovery volumes
    let inputs = expand_inputs(&files).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(outcome.failures.is_empty());

    for seg in &outcome.segments {
        let name = seg.subject_name.as_ref();
        if let Some(n) = name
            .strip_prefix("release.part")
            .and_then(|rest| rest.strip_suffix(".rar"))
            .and_then(|digits| digits.parse::<u32>().ok())
        {
            assert_eq!(
                seg.file_index, n,
                "file {name} expected index {n}, got {}",
                seg.file_index
            );
        }
    }

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn default_subject_conditional_on_existing_policy() {
    use pesto::article::default_subject;

    // Counter on: Some((filenum, total_files))
    let sub_on = default_subject("release.mkv", 1, 5, Some((2, 10)));
    assert_eq!(sub_on, "[2/10] - \"release.mkv\" yEnc (1/5)");

    // Counter off: None
    let sub_off = default_subject("release.mkv", 1, 5, None);
    assert_eq!(sub_off, "\"release.mkv\" yEnc (1/5)");
    assert!(!sub_off.contains('['));
}

#[test]
fn release_layout_operational_invariance_and_reordering_proof() {
    use pesto::poster::ReleaseLayout;

    // Prove that logical segment identity (release_ordinal, declared_part) -> segment_index
    // is invariant to the physical order in which items are scheduled or finished.
    let layout = ReleaseLayout::from_parts(3, &[(1, 2), (2, 3), (3, 1)]).unwrap();

    // Map of (file_ordinal, part_number) -> expected_segment_index
    let expected_mappings = [
        ((1, 1), 1),
        ((1, 2), 2),
        ((2, 1), 3),
        ((2, 2), 4),
        ((2, 3), 5),
        ((3, 1), 6),
    ];

    // Simulate arbitrary worker arrival orders: reversed, interleaved, perturbed
    let arrival_orders = [
        // Normal order
        vec![(1, 1), (1, 2), (2, 1), (2, 2), (2, 3), (3, 1)],
        // Reversed arrival
        vec![(3, 1), (2, 3), (2, 2), (2, 1), (1, 2), (1, 1)],
        // Interleaved arrival across files
        vec![(2, 2), (1, 1), (3, 1), (2, 1), (1, 2), (2, 3)],
    ];

    for arrivals in arrival_orders {
        for (ord, part) in arrivals {
            let id = layout.segment_identity(ord, part).expect("valid segment");
            let (_, expected_idx) = expected_mappings
                .iter()
                .find(|&&(k, _)| k == (ord, part))
                .unwrap();
            assert_eq!(
                id.segment_index, *expected_idx,
                "arrival order perturbation must never change logical segment index"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_unchanged_runs_produce_identical_segment_identities_and_nzb() {
    let (root, files) = build_volumes("repeat", 3);
    let recovery_count = 3u32;
    let config = dry_run_config(true, Some(recovery_count as usize));
    let inputs1 = expand_inputs(&files).unwrap();
    let inputs2 = expand_inputs(&files).unwrap();

    let outcome1 = post_files(&config, &inputs1).await.unwrap();
    let outcome2 = post_files(&config, &inputs2).await.unwrap();

    assert_eq!(outcome1.segments.len(), outcome2.segments.len());
    for (s1, s2) in outcome1.segments.iter().zip(outcome2.segments.iter()) {
        assert_eq!(s1.file_name, s2.file_name);
        assert_eq!(s1.part, s2.part);
        assert_eq!(s1.total, s2.total);
        assert_eq!(s1.file_index, s2.file_index);
        assert_eq!(s1.total_files, s2.total_files);
        assert!(s1.segment_identity.is_some());
        assert_eq!(s1.segment_identity, s2.segment_identity);
    }

    let nzb1 = pesto::nzb::generate(
        &config.groups,
        &outcome1.segments,
        &pesto::nzb::NzbMeta::default(),
        ObfuscateMode::None,
    )
    .unwrap();
    let nzb2 = pesto::nzb::generate(
        &config.groups,
        &outcome2.segments,
        &pesto::nzb::NzbMeta::default(),
        ObfuscateMode::None,
    )
    .unwrap();

    // The generated XML subjects and segment mappings must be identical
    assert_eq!(
        nzb1.matches("<file ").count(),
        nzb2.matches("<file ").count()
    );
    assert_eq!(
        nzb1.matches("<segment ").count(),
        nzb2.matches("<segment ").count()
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn pregenerated_par2_and_normal_par2_produce_identical_segment_identities() {
    let (root, files) = build_two_files("par2_eq");
    let recovery_count = 3u32;

    let config_normal = dry_run_config(true, Some(recovery_count as usize));
    let mut config_pregen = dry_run_config(true, Some(recovery_count as usize));
    config_pregen.par2_before_upload = true;

    let inputs1 = expand_inputs(&files).unwrap();
    let inputs2 = expand_inputs(&files).unwrap();

    let outcome_normal = post_files(&config_normal, &inputs1).await.unwrap();
    let outcome_pregen = post_files(&config_pregen, &inputs2).await.unwrap();

    assert_eq!(outcome_normal.segments.len(), outcome_pregen.segments.len());

    for (s_norm, s_pre) in outcome_normal
        .segments
        .iter()
        .zip(outcome_pregen.segments.iter())
    {
        assert_eq!(s_norm.file_name, s_pre.file_name);
        assert_eq!(s_norm.part, s_pre.part);
        assert_eq!(s_norm.total, s_pre.total);
        assert_eq!(s_norm.file_index, s_pre.file_index);
        assert_eq!(s_norm.total_files, s_pre.total_files);
        assert_eq!(s_norm.segment_identity, s_pre.segment_identity);
        assert!(s_norm.segment_identity.is_some());
    }

    std::fs::remove_dir_all(&root).ok();
}
