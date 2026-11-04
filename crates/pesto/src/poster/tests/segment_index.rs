use std::sync::Arc;

use super::*;
use crate::poster::prepare::ReleaseLayout;
use crate::poster::task::PostTask;
use crate::poster::FileMeta;

#[test]
fn release_layout_assigns_prefix_base_by_natural_ordinal_not_processing_order() {
    // 2 files in release: ordinal 1 has 1 part, ordinal 2 has 2 parts.
    // Inputs passed out of ordinal order (e.g. File-ID sort order: ordinal 2, then ordinal 1).
    let parts = vec![(2, 2), (1, 1)];
    let layout = ReleaseLayout::from_parts(2, &parts).expect("valid layout");

    assert_eq!(layout.total_files, 2);

    let entry1 = layout.entry(1).expect("entry for ordinal 1");
    assert_eq!(entry1.part_count, 1);
    assert_eq!(entry1.prefix_parts, 0);

    let entry2 = layout.entry(2).expect("entry for ordinal 2");
    assert_eq!(entry2.part_count, 2);
    assert_eq!(entry2.prefix_parts, 1);

    // Segment indices:
    assert_eq!(layout.segment_identity(1, 1).expect("part 1 of file 1"), 1);
    assert_eq!(layout.segment_identity(2, 1).expect("part 1 of file 2"), 2);
    assert_eq!(layout.segment_identity(2, 2).expect("part 2 of file 2"), 3);

    // Out of bounds queries return None
    assert!(layout.segment_identity(1, 2).is_none());
    assert!(layout.segment_identity(2, 3).is_none());
    assert!(layout.segment_identity(0, 1).is_none());
    assert!(layout.segment_identity(3, 1).is_none());
}

#[test]
fn release_layout_planned_par2_index_and_volumes_follow_data_files() {
    // 2 data files:
    // File 1 (ordinal 1): 3 parts
    // File 2 (ordinal 2): 2 parts
    // PAR2 index (ordinal 3): 1 part
    // PAR2 vol 1 (ordinal 4): 2 parts
    // PAR2 vol 2 (ordinal 5): 4 parts
    let parts = vec![(1, 3), (2, 2), (3, 1), (4, 2), (5, 4)];
    let layout = ReleaseLayout::from_parts(5, &parts).expect("valid layout with PAR2");

    assert_eq!(layout.total_files, 5);

    // Ordinal 1: prefix 0, segments 1..=3
    assert_eq!(layout.entry(1).unwrap().prefix_parts, 0);
    assert_eq!(layout.segment_identity(1, 1).unwrap(), 1);
    assert_eq!(layout.segment_identity(1, 3).unwrap(), 3);

    // Ordinal 2: prefix 3, segments 4..=5
    assert_eq!(layout.entry(2).unwrap().prefix_parts, 3);
    assert_eq!(layout.segment_identity(2, 1).unwrap(), 4);
    assert_eq!(layout.segment_identity(2, 2).unwrap(), 5);

    // PAR2 index (ordinal 3): prefix 5, segment 6
    assert_eq!(layout.entry(3).unwrap().prefix_parts, 5);
    assert_eq!(layout.segment_identity(3, 1).unwrap(), 6);

    // PAR2 volume 1 (ordinal 4): prefix 6, segments 7..=8
    assert_eq!(layout.entry(4).unwrap().prefix_parts, 6);
    assert_eq!(layout.segment_identity(4, 1).unwrap(), 7);
    assert_eq!(layout.segment_identity(4, 2).unwrap(), 8);

    // PAR2 volume 2 (ordinal 5): prefix 8, segments 9..=12
    assert_eq!(layout.entry(5).unwrap().prefix_parts, 8);
    assert_eq!(layout.segment_identity(5, 1).unwrap(), 9);
    // Index framing rule: ranks 10 and 12 map through nth_safe_segment_index, skipping
    // forbidden indices 10 and 13 (assigned 11 and 14).
    assert_eq!(layout.segment_identity(5, 2).unwrap(), 11);
    assert_eq!(layout.segment_identity(5, 4).unwrap(), 14);
}

#[test]
fn release_layout_fails_on_zero_parts() {
    let err = ReleaseLayout::from_parts(1, &[(1, 0)]).unwrap_err();
    assert!(err.to_string().contains("zero parts"));
}

#[test]
fn release_layout_fails_on_cumulative_overflow_or_u32_max() {
    let err = ReleaseLayout::from_parts(2, &[(1, u32::MAX), (2, 1)]).unwrap_err();
    assert!(
        err.to_string().contains("exceeds u32::MAX")
            || err.to_string().contains("overflow")
            || err.to_string().contains("safe segment-index capacity")
    );
}

#[test]
fn release_layout_safe_index_overflow_fails_cleanly() {
    // Index framing rule: a release whose highest rank's safe index would exceed u32::MAX
    // fails layout construction with an error (never a panic) even though the
    // raw cumulative count itself fits u32::MAX.
    let err = ReleaseLayout::from_parts(1, &[(1, u32::MAX)]).unwrap_err();
    assert!(
        err.to_string().contains("safe segment-index capacity"),
        "expected safe-index capacity error, got: {err}"
    );
}

#[test]
fn release_layout_fails_on_missing_or_duplicate_ordinals() {
    assert!(ReleaseLayout::from_parts(0, &[]).is_err());
    assert!(ReleaseLayout::from_parts(2, &[(1, 1), (3, 1)]).is_err());
    assert!(ReleaseLayout::from_parts(2, &[(1, 1), (1, 2)]).is_err());
    assert!(ReleaseLayout::from_parts(2, &[(1, 1)]).is_err());
}

#[test]
fn commit_result_records_planned_segment_index() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("file.bin");
    std::fs::write(&file_path, b"test payload").unwrap();

    let meta = Arc::new(FileMeta {
        path: file_path.clone(),
        real_name: "file.bin".into(),
        client_path: "file.bin".into(),
        subject_name: "file.bin".into(),
        yenc_name: "file.bin".into(),
        from: "test <t@x>".into(),
        date: (None, None),
        size: 12,
        file_index: 1,
        fingerprint: crate::resume::FileFingerprint {
            size: 12,
            mtime: None,
        },
    });

    let task = PostTask {
        meta: Arc::clone(&meta),
        part: 1,
        total: 1,
        offset: 0,
        data: b"test payload".to_vec(),
        segment_index: 1,
        plaintext_digest: None,
        subject_name: "file.bin".into(),
        yenc_name: "file.bin".into(),
        from: "test <t@x>".into(),
        date: (None, None),
        file_crc32: Some(12345),
    };

    let shared = minimal_shared(1024);

    // Commit success
    super::super::result::commit_result(
        &shared,
        task,
        "<msg@x>".into(),
        100,
        true,
        "",
        (None, None),
        0,
    );

    let results = shared.results.lock().unwrap();
    assert_eq!(results.len(), 1);

    // Commit failure records a FailedTask carrying the planned segment index
    let task_fail = PostTask {
        meta: Arc::clone(&meta),
        part: 1,
        total: 1,
        offset: 0,
        data: b"fail payload".to_vec(),
        segment_index: 1,
        plaintext_digest: None,
        subject_name: "file.bin".into(),
        yenc_name: "file.bin".into(),
        from: "test <t@x>".into(),
        date: (None, None),
        file_crc32: Some(12345),
    };

    super::super::result::commit_result(
        &shared,
        task_fail,
        "<msg-fail@x>".into(),
        100,
        false,
        "error 500",
        (None, None),
        0,
    );

    let failed = shared.failed_tasks.lock().unwrap();
    assert_eq!(failed.len(), 1);
    let failed_task = &failed[0];
    assert_eq!(failed_task.segment_index, 1);
}

#[test]
fn encrypted_layout_refuses_four_gib_before_article_production() {
    let mut meta = meta_with_name(std::path::Path::new("huge.bin"), "huge.bin");
    meta.size = 1u64 << 32;
    let err = ReleaseLayout::build(&[Arc::new(meta)], &dry_run_config(), 0, 2048).unwrap_err();
    assert!(err.to_string().contains("2^32"));
}

#[test]
fn encrypted_layout_accepts_largest_encrypted_size() {
    // GAP13-04 (boundary companion to the 4-GiB refusal above): the largest
    // legal encrypted file size, 2^32 - 1, must build successfully. Metadata
    // only — no multi-GiB file is allocated.
    let mut meta = meta_with_name(std::path::Path::new("huge.bin"), "huge.bin");
    meta.size = (1u64 << 32) - 1;
    ReleaseLayout::build(&[Arc::new(meta)], &dry_run_config(), 0, 2048)
        .expect("largest encrypted size (2^32 - 1) must be accepted");
}

#[test]
fn layout_refuses_oversized_generated_par2_volumes() {
    let metas: Vec<_> = (0..4)
        .map(|index| {
            let name = format!("file{index}.bin");
            let mut meta = meta_with_name(std::path::Path::new(&name), &name);
            meta.size = 3 * (1u64 << 30);
            meta.file_index = index + 1;
            Arc::new(meta)
        })
        .collect();
    let mut config = dry_run_config();
    config.par2 = 100;
    config.par2_slice_size = Some(1 << 20);
    let (slice_size, slices, recovery) = crate::poster::par2::par2_geometry(&metas, &config);
    assert_eq!((slice_size, slices, recovery), (1 << 20, 12288, 12288));
    for mode in [
        crate::config::ObfuscateMode::None,
        crate::config::ObfuscateMode::Full,
    ] {
        config.obfuscate = mode;
        let err = ReleaseLayout::build(&metas, &config, recovery, slice_size).unwrap_err();
        assert!(
            err.to_string().contains("PAR2 volume") && err.to_string().contains("2^32"),
            "{err}"
        );
    }
    // Smaller recovery plans remain accepted with identical data-file limits.
    assert!(ReleaseLayout::build(&metas, &config, 5, slice_size).is_ok());
}

#[test]
fn layout_refuses_oversized_published_par2_index() {
    let metas: Vec<_> = (0..5)
        .map(|index| {
            let name = format!("file{index}.bin");
            let mut meta = meta_with_name(std::path::Path::new(&name), &name);
            meta.size = 3 * (1u64 << 30);
            meta.file_index = index + 1;
            Arc::new(meta)
        })
        .collect();
    // Exercise the planner's index bound directly with metadata only;
    // no multi-GiB files or checksum arrays are allocated.
    assert!(prepare::par2_base_packets_len(&metas, 64) >= 1u64 << 32);
    let err = ReleaseLayout::build(&metas, &dry_run_config(), 1, 64).unwrap_err();
    assert!(
        err.to_string().contains("PAR2 index") && err.to_string().contains("2^32"),
        "{err}"
    );
}
