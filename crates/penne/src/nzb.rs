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
                file_path: "a.bin".into(),
                subject_name: "a.bin".into(),
                file_size: 1000,
                part: 1,
                total: 2,
                message_id: "<a1@x>".into(),
                bytes: 500,
                from: "poster <p@x>".into(),
                date: (None, None),
                full_crc32: 0,
                server_idx: 0,
            },
            pesto::poster::PostedSegment {
                file_name: "a.bin".into(),
                file_path: "a.bin".into(),
                subject_name: "a.bin".into(),
                file_size: 1000,
                part: 2,
                total: 2,
                message_id: "<a2@x>".into(),
                bytes: 500,
                from: "poster <p@x>".into(),
                date: (None, None),
                full_crc32: 0,
                server_idx: 0,
            },
        ];
        let xml = pesto::nzb::generate(&groups, &segments, &pesto::nzb::NzbMeta::default());

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(xml.as_bytes()).unwrap();

        let parsed = load(file.path()).unwrap();
        let summary = summarize(&parsed);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.segments, 2);
        assert_eq!(summary.total_bytes, 1000);
    }
}
