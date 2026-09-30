//! Loading `.nzb` files.
//!
//! Parsing itself is not reimplemented here — [`pesto::nzb::parse`] already
//! does it (it is the same format `pesto` writes when posting). This module
//! adds the download-side conveniences: reading from disk and summarizing.

use std::path::Path;

use anyhow::{Context, Result};
use pesto::nzb::ParsedNzb;

/// Read and parse a `.nzb` file from disk.
pub fn load(path: &Path) -> Result<ParsedNzb> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    pesto::nzb::parse(&contents).with_context(|| format!("parsing {}", path.display()))
}

/// Read and parse an encrypted `.nzb` file from disk, strictly enforcing
/// explicit `segmentIndex` on every segment.
pub fn load_encrypted(path: &Path) -> Result<ParsedNzb> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    pesto::nzb::parse_encrypted(&contents).with_context(|| format!("parsing {}", path.display()))
}

/// Construct a transport decryption adapter from NZB metadata.
///
/// Decryption is activated strictly when `meta.yenc_encrypted` is true.
/// If `meta.yenc_encrypted` is false (e.g. unencrypted NZBs or NZBs with an archive password only),
/// returns `Ok(None)`.
/// If `meta.yenc_encrypted` is true, requires `meta.password`.
/// The password is never printed in error messages.
pub fn download_decryptor(
    meta: &pesto::nzb::NzbMeta,
) -> Result<Option<std::sync::Arc<pesto::crypto::DownloadDecryptionAdapter>>> {
    if !meta.yenc_encrypted {
        return Ok(None);
    }
    let password = meta
        .password
        .as_deref()
        .context("encrypted NZB is missing password metadata")?;
    Ok(Some(std::sync::Arc::new(
        pesto::crypto::DownloadDecryptionAdapter::with_password(password),
    )))
}

/// Aggregate counts over a parsed `.nzb`, used for `penne info` and for the
/// pre-download summary printed before a download starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub segments: usize,
    pub total_bytes: u64,
}

/// Compute a [`Summary`] over every segment in a parsed `.nzb`.
pub fn summarize(parsed: &ParsedNzb) -> Summary {
    let mut files = std::collections::HashSet::new();
    let mut total_bytes = 0u64;
    for seg in &parsed.segments {
        files.insert(&seg.file_name);
        total_bytes += seg.bytes;
    }
    Summary {
        files: files.len(),
        segments: parsed.segments.len(),
        total_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn summarize_counts_files_and_segments() {
        let groups = vec!["alt.test".to_string()];
        let segments = vec![
            pesto::poster::PostedSegment {
                file_name: "a.bin".into(),
                file_path: Path::new("a.bin").into(),
                subject_name: "a.bin".into(),
                wire_name: "a.bin".into(),
                wire_yenc_name: "a.bin".into(),
                file_size: 1000,
                part: 1,
                total: 2,
                message_id: "<a1@x>".into(),
                bytes: 500,
                from: "poster <p@x>".into(),
                date: (None, None),
                full_crc32: 0,
                server_idx: 0,
                file_index: 0,
                total_files: 0,
                segment_identity: None,
            },
            pesto::poster::PostedSegment {
                file_name: "a.bin".into(),
                file_path: Path::new("a.bin").into(),
                subject_name: "a.bin".into(),
                wire_name: "a.bin".into(),
                wire_yenc_name: "a.bin".into(),
                file_size: 1000,
                part: 2,
                total: 2,
                message_id: "<a2@x>".into(),
                bytes: 500,
                from: "poster <p@x>".into(),
                date: (None, None),
                full_crc32: 0,
                server_idx: 0,
                file_index: 0,
                total_files: 0,
                segment_identity: None,
            },
        ];
        let xml = pesto::nzb::generate(
            &groups,
            &segments,
            &pesto::nzb::NzbMeta::default(),
            pesto::config::ObfuscateMode::None,
        )
        .unwrap();

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(xml.as_bytes()).unwrap();

        let parsed = load(file.path()).unwrap();
        let summary = summarize(&parsed);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.segments, 2);
        assert_eq!(summary.total_bytes, 1000);
    }

    #[test]
    fn test_download_decryptor_mode_selection() {
        // 1. Unencrypted metadata yields None even if password is set (archive password only)
        let meta_unencrypted = pesto::nzb::NzbMeta {
            yenc_encrypted: false,
            password: Some("archive-secret".into()),
            ..Default::default()
        };
        let decryptor = download_decryptor(&meta_unencrypted).unwrap();
        assert!(decryptor.is_none());

        // 2. Encrypted metadata without password errors fail-closed
        let meta_missing_pwd = pesto::nzb::NzbMeta {
            yenc_encrypted: true,
            password: None,
            ..Default::default()
        };
        let err = match download_decryptor(&meta_missing_pwd) {
            Err(e) => e,
            Ok(_) => panic!("expected error for missing password on encrypted NZB"),
        };
        assert!(err.to_string().contains("missing password metadata"));

        // 3. Encrypted metadata with password creates decryptor
        let meta_encrypted = pesto::nzb::NzbMeta {
            yenc_encrypted: true,
            password: Some("transport-secret".into()),
            ..Default::default()
        };
        let decryptor = download_decryptor(&meta_encrypted).unwrap();
        assert!(decryptor.is_some());
    }

    #[test]
    fn test_validate_queue_identity_unencrypted_allows_missing_or_arbitrary() {
        let queue = crate::queue::DownloadQueue {
            files: vec![crate::queue::QueuedFile {
                name: "file.bin".into(),
                segments: vec![
                    crate::queue::QueuedSegment {
                        message_id: "id1@x".into(),
                        part: 1,
                        bytes: 100,
                        segment_index: None,
                    },
                    crate::queue::QueuedSegment {
                        message_id: "id2@x".into(),
                        part: 2,
                        bytes: 100,
                        segment_index: Some(0),
                    },
                ],
                file_ordinal: None,
                total_files: None,
            }],
        };
        assert!(crate::download::validate_queue_identity(&queue, false).is_ok());
    }

    #[test]
    fn test_validate_queue_identity_encrypted_fail_closed_checks() {
        // Valid encrypted queue with sparse/unordered indices
        let valid_queue = crate::queue::DownloadQueue {
            files: vec![
                crate::queue::QueuedFile {
                    name: "file1.bin".into(),
                    segments: vec![crate::queue::QueuedSegment {
                        message_id: "id1@x".into(),
                        part: 1,
                        bytes: 100,
                        segment_index: Some(10),
                    }],
                    file_ordinal: None,
                    total_files: None,
                },
                crate::queue::QueuedFile {
                    name: "file2.bin".into(),
                    segments: vec![crate::queue::QueuedSegment {
                        message_id: "id2@x".into(),
                        part: 1,
                        bytes: 100,
                        segment_index: Some(5),
                    }],
                    file_ordinal: None,
                    total_files: None,
                },
            ],
        };
        assert!(crate::download::validate_queue_identity(&valid_queue, true).is_ok());

        // 1. Missing segment index
        let mut q_missing = valid_queue.clone();
        q_missing.files[0].segments[0].segment_index = None;
        let err = crate::download::validate_queue_identity(&q_missing, true).unwrap_err();
        assert!(err.to_string().contains("MISSING_SEGMENT_INDEX"));

        // 2. Invalid segment index zero
        let mut q_zero = valid_queue.clone();
        q_zero.files[0].segments[0].segment_index = Some(0);
        let err = crate::download::validate_queue_identity(&q_zero, true).unwrap_err();
        assert!(err.to_string().contains("INVALID_SEGMENT_INDEX_ZERO"));

        // 3. Duplicate segment index across different Message-IDs
        let mut q_dup = valid_queue.clone();
        q_dup.files[1].segments[0].segment_index = Some(10);
        let err = crate::download::validate_queue_identity(&q_dup, true).unwrap_err();
        assert!(err.to_string().contains("DUPLICATE_SEGMENT_INDEX"));

        // 4. Conflicting index for same Message-ID
        let q_conflict = crate::queue::DownloadQueue {
            files: vec![
                crate::queue::QueuedFile {
                    name: "file1.bin".into(),
                    segments: vec![crate::queue::QueuedSegment {
                        message_id: "same@x".into(),
                        part: 1,
                        bytes: 100,
                        segment_index: Some(1),
                    }],
                    file_ordinal: None,
                    total_files: None,
                },
                crate::queue::QueuedFile {
                    name: "file2.bin".into(),
                    segments: vec![crate::queue::QueuedSegment {
                        message_id: "same@x".into(),
                        part: 2,
                        bytes: 100,
                        segment_index: Some(2),
                    }],
                    file_ordinal: None,
                    total_files: None,
                },
            ],
        };
        let err = crate::download::validate_queue_identity(&q_conflict, true).unwrap_err();
        assert!(err.to_string().contains("CONFLICTING_MESSAGE_ID_INDEX"));
    }

    #[tokio::test]
    async fn test_download_queue_encrypted_without_decryptor_fails_preflight() {
        let queue = crate::queue::DownloadQueue {
            files: vec![crate::queue::QueuedFile {
                name: "file.bin".into(),
                segments: vec![crate::queue::QueuedSegment {
                    message_id: "msg@x".into(),
                    part: 1,
                    bytes: 100,
                    segment_index: Some(1),
                }],
                file_ordinal: None,
                total_files: None,
            }],
        };
        let dest = tempfile::tempdir().unwrap();
        let dummy_tier = crate::config::ServerTier::solo(pesto::config::ServerEntry {
            host: "127.0.0.1".into(),
            port: 119,
            ssl: false,
            connections: 1,
            username: None,
            password: None,
            retry_delay: 0,
            timeout: 1,
            proxy: None,
        });
        let res =
            crate::download::download_queue(&queue, &[dummy_tier], dest.path(), 0, None).await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err
            .contains("queue contains encrypted segments but no decryption adapter was provided"));
    }
}
