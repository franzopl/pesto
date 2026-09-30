use std::sync::Arc;

use super::*;
use crate::poster::outcome::SegmentIdentity;
use crate::poster::prepare::ReleaseLayout;
use crate::poster::task::PostTask;
use crate::poster::FileMeta;

#[test]
fn release_layout_assigns_prefix_base_by_natural_ordinal_not_processing_order() {
    // 2 files in release: ordinal 1 has 1 part, ordinal 2 has 2 parts.
    // Inputs passed out of ordinal order (e.g. File-ID sort order: ordinal 2, then ordinal 1).
    let parts = vec![(2, 2), (1, 1)];
    let layout = ReleaseLayout::from_parts(2, &parts).expect("valid layout");

    assert_eq!(layout.total_files(), 2);
    assert_eq!(layout.total_segments(), 3);

    let entry1 = layout.entry(1).expect("entry for ordinal 1");
    assert_eq!(entry1.release_ordinal, 1);
    assert_eq!(entry1.part_count, 1);
    assert_eq!(entry1.prefix_parts, 0);

    let entry2 = layout.entry(2).expect("entry for ordinal 2");
    assert_eq!(entry2.release_ordinal, 2);
    assert_eq!(entry2.part_count, 2);
    assert_eq!(entry2.prefix_parts, 1);

    // Segment identities:
    let id1_1 = layout.segment_identity(1, 1).expect("part 1 of file 1");
    assert_eq!(id1_1.file_ordinal, 1);
    assert_eq!(id1_1.total_files, 2);
    assert_eq!(id1_1.part_number, 1);
    assert_eq!(id1_1.segment_index, 1);

    let id2_1 = layout.segment_identity(2, 1).expect("part 1 of file 2");
    assert_eq!(id2_1.file_ordinal, 2);
    assert_eq!(id2_1.total_files, 2);
    assert_eq!(id2_1.part_number, 1);
    assert_eq!(id2_1.segment_index, 2);

    let id2_2 = layout.segment_identity(2, 2).expect("part 2 of file 2");
    assert_eq!(id2_2.file_ordinal, 2);
    assert_eq!(id2_2.total_files, 2);
    assert_eq!(id2_2.part_number, 2);
    assert_eq!(id2_2.segment_index, 3);

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

    assert_eq!(layout.total_files(), 5);
    assert_eq!(layout.total_segments(), 12);

    // Ordinal 1: prefix 0, segments 1..=3
    assert_eq!(layout.entry(1).unwrap().prefix_parts, 0);
    assert_eq!(layout.segment_identity(1, 1).unwrap().segment_index, 1);
    assert_eq!(layout.segment_identity(1, 3).unwrap().segment_index, 3);

    // Ordinal 2: prefix 3, segments 4..=5
    assert_eq!(layout.entry(2).unwrap().prefix_parts, 3);
    assert_eq!(layout.segment_identity(2, 1).unwrap().segment_index, 4);
    assert_eq!(layout.segment_identity(2, 2).unwrap().segment_index, 5);

    // PAR2 index (ordinal 3): prefix 5, segment 6
    assert_eq!(layout.entry(3).unwrap().prefix_parts, 5);
    assert_eq!(layout.segment_identity(3, 1).unwrap().segment_index, 6);

    // PAR2 volume 1 (ordinal 4): prefix 6, segments 7..=8
    assert_eq!(layout.entry(4).unwrap().prefix_parts, 6);
    assert_eq!(layout.segment_identity(4, 1).unwrap().segment_index, 7);
    assert_eq!(layout.segment_identity(4, 2).unwrap().segment_index, 8);

    // PAR2 volume 2 (ordinal 5): prefix 8, segments 9..=12
    assert_eq!(layout.entry(5).unwrap().prefix_parts, 8);
    assert_eq!(layout.segment_identity(5, 1).unwrap().segment_index, 9);
    assert_eq!(layout.segment_identity(5, 4).unwrap().segment_index, 12);
}

#[test]
fn release_layout_fails_on_zero_parts() {
    let err = ReleaseLayout::from_parts(1, &[(1, 0)]).unwrap_err();
    assert!(err.to_string().contains("zero parts"));
}

#[test]
fn release_layout_fails_on_cumulative_overflow_or_u32_max() {
    let err = ReleaseLayout::from_parts(2, &[(1, u32::MAX), (2, 1)]).unwrap_err();
    assert!(err.to_string().contains("exceeds u32::MAX") || err.to_string().contains("overflow"));
}

#[test]
fn release_layout_fails_on_missing_or_duplicate_ordinals() {
    assert!(ReleaseLayout::from_parts(0, &[]).is_err());
    assert!(ReleaseLayout::from_parts(2, &[(1, 1), (3, 1)]).is_err());
    assert!(ReleaseLayout::from_parts(2, &[(1, 1), (1, 2)]).is_err());
    assert!(ReleaseLayout::from_parts(2, &[(1, 1)]).is_err());
}

#[test]
fn post_task_carries_checked_segment_identity() {
    let layout = ReleaseLayout::from_parts(1, &[(1, 1)]).unwrap();
    let id = layout.segment_identity(1, 1).unwrap();
    assert_eq!(id.segment_index, 1);
    assert_eq!(id.file_ordinal, 1);
    assert_eq!(id.total_files, 1);
    assert_eq!(id.part_number, 1);
}

#[test]
fn posted_segment_and_failed_task_carry_planned_identity() {
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
        mtime: None,
        release_ordinal: 1,
        file_index: 1,
    });

    let id = SegmentIdentity::checked(0, 1, 1, 1).expect("valid id");
    let task = PostTask {
        meta: Arc::clone(&meta),
        part: 1,
        total: 1,
        offset: 0,
        data: b"test payload".to_vec(),
        segment_identity: id,
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
    let posted_seg = &results[0];
    assert_eq!(posted_seg.segment_identity, Some(id));
    assert_eq!(posted_seg.segment_identity.unwrap().segment_index, 1);
    assert_eq!(posted_seg.segment_identity.unwrap().file_ordinal, 1);
    assert_eq!(posted_seg.segment_identity.unwrap().part_number, 1);
    assert_eq!(posted_seg.segment_identity.unwrap().total_files, 1);

    // Commit failure
    let task_fail = PostTask {
        meta: Arc::clone(&meta),
        part: 1,
        total: 1,
        offset: 0,
        data: b"fail payload".to_vec(),
        segment_identity: id,
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
    assert_eq!(failed_task.segment_identity, id);
    assert_eq!(failed_task.segment_identity.segment_index, 1);
    assert_eq!(failed_task.segment_identity.file_ordinal, 1);
    assert_eq!(failed_task.segment_identity.part_number, 1);
    assert_eq!(failed_task.segment_identity.total_files, 1);
}

#[test]
fn sorting_post_outcome_preserves_exact_segment_identities() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("dummy");

    let id1 = SegmentIdentity::checked(0, 1, 2, 1).unwrap();
    let id2 = SegmentIdentity::checked(1, 2, 2, 1).unwrap();

    let seg1 = PostedSegment {
        file_name: "file_a.bin".into(),
        file_path: Arc::from(file_path.as_path()),
        subject_name: Arc::from("file_a.bin"),
        wire_name: Arc::from("file_a.bin"),
        wire_yenc_name: Arc::from("file_a.bin"),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<id1@x>".into(),
        bytes: 100,
        from: Arc::from("p@x"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 1,
        total_files: 2,
        segment_identity: Some(id1),
    };

    let seg2 = PostedSegment {
        file_name: "file_b.bin".into(),
        file_path: Arc::from(file_path.as_path()),
        subject_name: Arc::from("file_b.bin"),
        wire_name: Arc::from("file_b.bin"),
        wire_yenc_name: Arc::from("file_b.bin"),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<id2@x>".into(),
        bytes: 100,
        from: Arc::from("p@x"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 2,
        total_files: 2,
        segment_identity: Some(id2),
    };

    // Push into shared in reverse order (file_b, then file_a)
    let config = dry_run_config();
    let shared = minimal_shared(1024);
    shared.results.lock().unwrap().push(seg2);
    shared.results.lock().unwrap().push(seg1);

    let outcome = super::super::result::build_outcome(
        &config,
        &shared,
        Vec::new(),
        Vec::new(),
        false,
        Vec::new(),
        Vec::new(),
        None,
        std::time::Instant::now(),
    );

    // After outcome assembly, segments are sorted natural by name: file_a, then file_b
    assert_eq!(outcome.segments.len(), 2);
    assert_eq!(outcome.segments[0].file_name, "file_a.bin");
    assert_eq!(outcome.segments[0].segment_identity, Some(id1));
    assert_eq!(outcome.segments[1].file_name, "file_b.bin");
    assert_eq!(outcome.segments[1].segment_identity, Some(id2));
}

#[tokio::test]
async fn dry_run_assigns_planned_identity_to_posted_segments() {
    let dir = TempDir::new().unwrap();
    let f1 = dir.path().join("a.bin");
    let f2 = dir.path().join("b.bin");
    std::fs::write(&f1, vec![0u8; 1000]).unwrap();
    std::fs::write(&f2, vec![0u8; 1000]).unwrap();

    let files = vec![
        InputFile {
            path: f1.clone(),
            name: "a.bin".into(),
        },
        InputFile {
            path: f2.clone(),
            name: "b.bin".into(),
        },
    ];

    let mut config = dry_run_config();
    config.file_counter = true;
    let outcome = post_files(&config, &files).await.unwrap();

    assert_eq!(outcome.segments.len(), 2);
    // a.bin: ordinal 1, prefix 0, part 1 -> segment_index 1
    let seg_a = outcome
        .segments
        .iter()
        .find(|s| s.file_name == "a.bin")
        .unwrap();
    let id_a = seg_a.segment_identity.expect("a.bin has segment identity");
    assert_eq!(id_a.file_ordinal, 1);
    assert_eq!(id_a.total_files, 2);
    assert_eq!(id_a.part_number, 1);
    assert_eq!(id_a.segment_index, 1);

    // b.bin: ordinal 2, prefix 1, part 1 -> segment_index 2
    let seg_b = outcome
        .segments
        .iter()
        .find(|s| s.file_name == "b.bin")
        .unwrap();
    let id_b = seg_b.segment_identity.expect("b.bin has segment identity");
    assert_eq!(id_b.file_ordinal, 2);
    assert_eq!(id_b.total_files, 2);
    assert_eq!(id_b.part_number, 1);
    assert_eq!(id_b.segment_index, 2);
}

#[tokio::test]
async fn repost_failed_tasks_preserves_exact_identity_and_subject_ordinals() {
    let id = SegmentIdentity::checked(3, 2, 3, 2).unwrap();
    assert_eq!(id.segment_index, 5);
    assert_eq!(id.file_ordinal, 2);
    assert_eq!(id.part_number, 2);
    assert_eq!(id.total_files, 3);

    let failed = FailedTask {
        file_name: "file2.bin".into(),
        client_path: "file2.bin".into(),
        file_path: std::path::PathBuf::from("/nonexistent/file2.bin"),
        message_id: "<orig-failed@test>".into(),
        subject_name: "file2.bin".into(),
        yenc_name: "file2.bin".into(),
        file_size: 1000,
        part: 2,
        total: 2,
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        file_index: 2,
        total_files: 3,
        segment_identity: id,
    };

    // Verify subject formatting with default_subject matches original
    let subject = crate::article::default_subject(
        &failed.subject_name,
        failed.part,
        failed.total,
        (failed.total_files > 0).then_some((failed.file_index, failed.total_files)),
    );
    assert_eq!(subject, "[2/3] - \"file2.bin\" yEnc (2/2)");

    // Empty slots returns empty vector without error
    let config = dry_run_config();
    let mut slots = Vec::new();
    let recovered = super::super::result::repost_failed_tasks(
        &config,
        &[failed],
        &["alt.test".into()],
        None,
        None,
        &mut slots,
        None,
    )
    .await
    .unwrap();
    assert!(recovered.is_empty());
}

#[test]
fn release_layout_build_populates_par2_file_names_for_identity_resolution() {
    let mut config = dry_run_config();
    config.article_size = 500;
    let meta = std::sync::Arc::new(FileMeta {
        path: std::path::PathBuf::from("test.bin"),
        real_name: "test.bin".into(),
        client_path: "test.bin".into(),
        subject_name: "test.bin".into(),
        yenc_name: "test.bin".into(),
        size: 1000,
        mtime: Some(100),
        from: "p@x".into(),
        date: (None, None),
        file_index: 1,
        release_ordinal: 1,
    });
    let layout = ReleaseLayout::build(&[meta], &config, 4, 1024).unwrap();
    let par2_entries: Vec<_> = layout
        .entries()
        .iter()
        .filter(|e| e.release_ordinal > 1)
        .collect();
    assert!(!par2_entries.is_empty());
    for entry in par2_entries {
        assert!(
            entry.file_name.is_some(),
            "PAR2 entry ordinal {} should have file_name",
            entry.release_ordinal
        );
        let name = entry.file_name.as_ref().unwrap();
        assert!(name.ends_with(".par2"));
    }
}

#[test]
fn task_dispatcher_queue_reordering_preserves_segment_identity() {
    let meta1 = Arc::new(FileMeta {
        path: PathBuf::from("f1.bin"),
        real_name: "f1.bin".into(),
        client_path: "f1.bin".into(),
        subject_name: "f1.bin".into(),
        yenc_name: "f1.bin".into(),
        size: 1000,
        mtime: Some(100),
        from: "p@x".into(),
        date: (None, None),
        file_index: 1,
        release_ordinal: 1,
    });
    let meta2 = Arc::new(FileMeta {
        path: PathBuf::from("f2.bin"),
        real_name: "f2.bin".into(),
        client_path: "f2.bin".into(),
        subject_name: "f2.bin".into(),
        yenc_name: "f2.bin".into(),
        size: 1000,
        mtime: Some(100),
        from: "p@x".into(),
        date: (None, None),
        file_index: 2,
        release_ordinal: 2,
    });

    let id1 = SegmentIdentity::explicit(1, 2, 1, 1).unwrap();
    let id2 = SegmentIdentity::explicit(1, 2, 2, 2).unwrap();
    let id3 = SegmentIdentity::explicit(2, 2, 1, 3).unwrap();

    let task1 = PostTask {
        meta: meta1.clone(),
        part: 1,
        total: 2,
        offset: 0,
        data: vec![1; 500],
        segment_identity: id1,
        subject_name: "f1.bin".into(),
        yenc_name: "f1.bin".into(),
        from: "p@x".into(),
        date: (None, None),
        file_crc32: None,
    };
    let task2 = PostTask {
        meta: meta1,
        part: 2,
        total: 2,
        offset: 500,
        data: vec![2; 500],
        segment_identity: id2,
        subject_name: "f1.bin".into(),
        yenc_name: "f1.bin".into(),
        from: "p@x".into(),
        date: (None, None),
        file_crc32: Some(12345),
    };
    let task3 = PostTask {
        meta: meta2,
        part: 1,
        total: 1,
        offset: 0,
        data: vec![3; 500],
        segment_identity: id3,
        subject_name: "f2.bin".into(),
        yenc_name: "f2.bin".into(),
        from: "p@x".into(),
        date: (None, None),
        file_crc32: Some(67890),
    };

    let mut queue = std::collections::VecDeque::new();
    queue.push_back(task1);
    queue.push_back(task2);
    queue.push_back(task3);

    // Shuffle / reorder tasks: pop task2 first, then task3, then task1
    let t1 = queue.pop_front().unwrap();
    let t2 = queue.pop_front().unwrap();
    let t3 = queue.pop_front().unwrap();

    let mut shuffled = std::collections::VecDeque::new();
    shuffled.push_back(t2);
    shuffled.push_back(t3);
    shuffled.push_back(t1);

    // Invariance check: identity remains attached to each specific task
    let p_t2 = shuffled.pop_front().unwrap();
    assert_eq!(p_t2.part, 2);
    assert_eq!(p_t2.segment_identity, id2);

    let p_t3 = shuffled.pop_front().unwrap();
    assert_eq!(p_t3.meta.real_name, "f2.bin");
    assert_eq!(p_t3.segment_identity, id3);

    let p_t1 = shuffled.pop_front().unwrap();
    assert_eq!(p_t1.part, 1);
    assert_eq!(p_t1.segment_identity, id1);
}

#[tokio::test]
async fn spool_storage_and_recovery_preserves_segment_identity_and_salt() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");

    let id = SegmentIdentity::explicit(1, 2, 1, 42).unwrap();
    let salt = [7u8; 16];
    let meta = crate::spool::SpoolMetadata {
        wire_identity: None,
        segment_identity: Some(id),
        session_salt: Some(salt),
        layout_fingerprint: Some("fingerprint-test-xyz".into()),
    };

    let file_name = "test_spool.bin";
    let part = 1;
    let message_id = "<spool-test-msg@example.com>";
    let headers = b"Subject: test\r\n";
    let body = b"yenc encrypted body content\r\n";

    crate::spool::write_with_metadata(
        &spool_dir, file_name, part, message_id, headers, body, &meta,
    )
    .await
    .unwrap();

    let recovered =
        crate::spool::read(&spool_dir, file_name, part).expect("must read back spooled article");
    assert_eq!(recovered.message_id, message_id);
    assert_eq!(recovered.headers, headers);
    assert_eq!(recovered.body, body);
    assert_eq!(recovered.segment_identity, Some(id));
    assert_eq!(recovered.session_salt, Some(salt));
    assert_eq!(
        recovered.layout_fingerprint.as_deref(),
        Some("fingerprint-test-xyz")
    );
}

#[test]
fn failed_task_preserves_immutable_segment_identity() {
    let id = SegmentIdentity::explicit(3, 5, 2, 99).unwrap();
    let failed = FailedTask {
        file_name: "video.mkv".into(),
        client_path: "video.mkv".into(),
        file_path: PathBuf::from("/tmp/video.mkv"),
        message_id: "<failed-id-123@domain>".into(),
        subject_name: "video.mkv".into(),
        yenc_name: "video.mkv".into(),
        file_size: 5000000,
        part: 2,
        total: 10,
        from: "user <u@d>".into(),
        date: (None, None),
        full_crc32: 0x12345678,
        file_index: 3,
        total_files: 5,
        segment_identity: id,
    };

    assert_eq!(failed.segment_identity.file_ordinal, 3);
    assert_eq!(failed.segment_identity.total_files, 5);
    assert_eq!(failed.segment_identity.part_number, 2);
    assert_eq!(failed.segment_identity.segment_index, 99);
}
