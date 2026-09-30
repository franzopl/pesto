//! Public API compatibility and regression verification tests.
//!
//! Confirms that public API signatures, struct layouts, and default implementations
//! remain backward compatible across releases.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pesto::config::{FileConfig, ObfuscateMode, Overrides};
use pesto::nzb::{generate, NzbMeta};
use pesto::poster::{FailedTask, PostedSegment, SegmentIdentity};

#[test]
fn test_nzb_meta_public_api_defaults() {
    let meta = NzbMeta::default();
    assert_eq!(meta.name, None);
    assert_eq!(meta.password, None);
    assert_eq!(meta.category, None);
    assert_eq!(meta.tmdb_id, None);
    assert_eq!(meta.imdb_id, None);
    assert_eq!(meta.tvdb_id, None);
    assert_eq!(meta.mal_id, None);
    assert!(meta.tags.is_empty());
    assert!(!meta.yenc_encrypted);
}

#[test]
fn test_posted_segment_public_api_construction() {
    let seg = PostedSegment {
        file_name: "test.bin".to_string(),
        file_path: Arc::from(Path::new("test.bin")),
        subject_name: Arc::from("test.bin"),
        wire_name: Arc::from("test.bin"),
        wire_yenc_name: Arc::from("test.bin"),
        file_size: 1024,
        part: 1,
        total: 1,
        message_id: "msg1@example.com".to_string(),
        bytes: 1024,
        from: Arc::from("poster@example.com"),
        date: (
            Some("Mon, 01 Jan 2026 00:00:00 +0000".to_string()),
            Some(1767225600),
        ),
        full_crc32: 0x12345678,
        server_idx: 0,
        file_index: 1,
        total_files: 1,
        segment_identity: SegmentIdentity::checked(0, 1, 1, 1),
    };

    assert_eq!(seg.file_name, "test.bin");
    assert_eq!(seg.part, 1);
    assert_eq!(seg.total, 1);
    assert_eq!(seg.bytes, 1024);
    assert!(seg.segment_identity.is_some());
}

#[test]
fn test_failed_task_public_api_construction() {
    let task = FailedTask {
        file_name: "test.bin".to_string(),
        client_path: "test.bin".to_string(),
        file_path: PathBuf::from("test.bin"),
        message_id: "msg1@example.com".to_string(),
        subject_name: "test.bin".to_string(),
        yenc_name: "test.bin".to_string(),
        file_size: 1024,
        part: 1,
        total: 1,
        from: "poster@example.com".to_string(),
        date: (
            Some("Mon, 01 Jan 2026 00:00:00 +0000".to_string()),
            Some(1767225600),
        ),
        full_crc32: 0x12345678,
        file_index: 1,
        total_files: 1,
        segment_identity: SegmentIdentity::checked(0, 1, 1, 1).unwrap(),
    };

    assert_eq!(task.file_name, "test.bin");
    assert_eq!(task.part, 1);
    assert_eq!(task.total, 1);
    assert_eq!(task.segment_identity.segment_index, 1);
}

#[test]
fn test_config_public_api_defaults() {
    let overrides = Overrides::default();
    assert_eq!(overrides.encrypt_password, None);
    assert_eq!(overrides.nzb_password, None);
    assert_eq!(overrides.compress_password, None);

    let file = FileConfig::default();
    assert_eq!(file.encryption.password, None);
    assert_eq!(file.output.nzb_password, None);
}

#[test]
fn test_nzb_generate_public_signature() {
    let groups = vec!["alt.binaries.test".to_string()];
    let seg = PostedSegment {
        file_name: "test.bin".to_string(),
        file_path: Arc::from(Path::new("test.bin")),
        subject_name: Arc::from("test.bin"),
        wire_name: Arc::from("test.bin"),
        wire_yenc_name: Arc::from("test.bin"),
        file_size: 1024,
        part: 1,
        total: 1,
        message_id: "msg1@example.com".to_string(),
        bytes: 1024,
        from: Arc::from("poster@example.com"),
        date: (
            Some("Mon, 01 Jan 2026 00:00:00 +0000".to_string()),
            Some(1767225600),
        ),
        full_crc32: 0x12345678,
        server_idx: 0,
        file_index: 0,
        total_files: 0,
        segment_identity: None,
    };
    let meta = NzbMeta::default();

    let result = generate(&groups, &[seg], &meta, ObfuscateMode::None);
    assert!(
        result.is_ok(),
        "NZB generation with default meta must succeed"
    );
    let xml = result.unwrap();
    assert!(xml.contains("<nzb"));
    assert!(!xml.contains("yenc_encrypted"));
}
