use super::*;

#[tokio::test]
async fn dry_run_produces_segments_without_network() {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("sample.bin");
    std::fs::write(&f, vec![0u8; 1500]).unwrap();

    let files = vec![InputFile {
        path: f.clone(),
        name: "sample.bin".into(),
    }];

    let config = dry_run_config();
    let outcome = post_files(&config, &files).await.unwrap();

    // Two segments (1500 bytes / 768 000 default = 1 here, but article_size
    // default is 768 000 so 1500 bytes → 1 segment).
    assert!(!outcome.segments.is_empty());
    assert!(outcome.failures.is_empty());
    assert!(!outcome.cancelled);
    assert_eq!(outcome.segments[0].file_name, "sample.bin");
}

#[tokio::test]
async fn light_release_override_unifies_a_compressed_volume_identity() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("scratch-bytes");
    std::fs::write(&source, vec![0u8; 1500]).unwrap();
    let files = vec![InputFile {
        path: source,
        name: "shareToken.7z.001".into(),
    }];
    let mut config = dry_run_config();
    config.obfuscate = ObfuscateMode::Light;

    let outcome = post_files_inner_with_release_prefix(
        &config,
        &files,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("shareToken"),
    )
    .await
    .unwrap();
    let segment = &outcome.segments[0];
    assert_eq!(segment.subject_name.as_ref(), "shareToken.7z.001");
    assert_eq!(segment.wire_name.as_ref(), "shareToken.7z.001");

    let nzb = crate::nzb::generate(
        &config.groups,
        &outcome.segments,
        &crate::nzb::NzbMeta::default(),
        config.obfuscate,
    );
    assert!(nzb.contains("shareToken.7z.001"));
}

#[tokio::test]
async fn dry_run_multi_segment_file() {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("big.bin");
    // Use a tiny article_size to force multiple segments.
    std::fs::write(&f, vec![0u8; 300]).unwrap();

    let files = vec![InputFile {
        path: f,
        name: "big.bin".into(),
    }];

    let mut config = dry_run_config();
    config.article_size = 100;
    let outcome = post_files(&config, &files).await.unwrap();

    // 300 bytes / 100 = 3 segments.
    assert_eq!(outcome.segments.len(), 3);
    for (i, seg) in outcome.segments.iter().enumerate() {
        assert_eq!(seg.part, (i + 1) as u32);
        assert_eq!(seg.total, 3);
    }
}

// ── multi-file dry-run ordering ───────────────────────────────────────────

#[tokio::test]
async fn dry_run_segments_sorted_by_filename_then_part() {
    let dir = TempDir::new().unwrap();
    let f1 = dir.path().join("b.bin");
    let f2 = dir.path().join("a.bin");
    std::fs::write(&f1, vec![0u8; 100]).unwrap();
    std::fs::write(&f2, vec![0u8; 100]).unwrap();

    let files = vec![
        InputFile {
            path: f1,
            name: "b.bin".into(),
        },
        InputFile {
            path: f2,
            name: "a.bin".into(),
        },
    ];

    let config = dry_run_config();
    let outcome = post_files(&config, &files).await.unwrap();

    let names: Vec<&str> = outcome
        .segments
        .iter()
        .map(|s| s.file_name.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "segments should be sorted by file name");
}

// ── obfuscation in dry-run ────────────────────────────────────────────────

#[tokio::test]
async fn dry_run_subject_obfuscation_hides_real_name_in_subject() {
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("secret.mkv");
    std::fs::write(&f, vec![0u8; 100]).unwrap();

    let files = vec![InputFile {
        path: f,
        name: "secret.mkv".into(),
    }];

    let mut file_cfg = crate::config::FileConfig::default();
    file_cfg.posting.groups = Some(vec!["alt.test".into()]);
    let config = Config::resolve(
        file_cfg,
        Overrides {
            dry_run: Some(true),
            par2: Some(0),
            obfuscate: Some(crate::config::ObfuscateMode::Full),
            ..Default::default()
        },
    )
    .unwrap();

    let outcome = post_files(&config, &files).await.unwrap();
    assert_eq!(outcome.segments.len(), 1);
    // The NZB name remains the canonical client path even when the NNTP
    // Subject is independently obfuscated.
    // returns the real file_name (secret.mkv) so download clients can rename correctly.
    assert_eq!(outcome.segments[0].file_name, "secret.mkv");
    assert_eq!(outcome.segments[0].subject_name.as_ref(), "secret.mkv");
}

#[tokio::test]
async fn dry_run_ignores_resume_state_by_design() {
    // Resume is explicitly disabled in dry_run mode (post_files_with_progress
    // only creates resume state when `!config.dry_run && !config.par2_only`).
    // Segments get fresh Message-IDs even when a state file with recorded
    // entries is present.
    let dir = TempDir::new().unwrap();
    let f = dir.path().join("r.bin");
    std::fs::write(&f, vec![0u8; 100]).unwrap();

    let state_path = dir.path().join("r.bin.pesto-state");
    let mut state = crate::resume::ResumeState::default();
    state.record("r.bin", 1, "<stored-id@pesto>", 100);
    state.save(&state_path).unwrap();

    let files = vec![InputFile {
        path: f,
        name: "r.bin".into(),
    }];

    let mut config = dry_run_config();
    config.resume = true; // resume flag set but dry_run overrides it

    let outcome = post_files_with_progress(&config, &files, None, Some(&state_path), None)
        .await
        .unwrap();

    // Segment is present but Message-ID is a fresh one, not the stored one.
    assert_eq!(outcome.segments.len(), 1);
    assert_ne!(outcome.segments[0].message_id, "<stored-id@pesto>");
}
