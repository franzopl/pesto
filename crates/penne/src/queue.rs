//! Download queue: the in-memory work list built from a parsed `.nzb`.
//!
//! This is pure data — no I/O. [`client`](crate::client) drains it against
//! NNTP connections; [`assemble`](crate::assemble) consumes the fetched
//! bodies. Kept separate so the queue itself is trivially testable without a
//! server.

use pesto::nzb::ParsedNzb;

/// One article to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedSegment {
    pub message_id: String,
    pub part: u32,
    pub bytes: u64,
}

/// One file to reassemble, and the segments it is made of, in part order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedFile {
    pub name: String,
    pub segments: Vec<QueuedSegment>,
}

/// The full set of files/segments to download for one `.nzb`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DownloadQueue {
    pub files: Vec<QueuedFile>,
}

/// Build a [`DownloadQueue`] from a parsed `.nzb`.
///
/// `parsed.segments` is already sorted by `(file_name, part)` (see
/// [`pesto::nzb::parse`]), so consecutive runs sharing a file name are
/// grouped into one [`QueuedFile`].
pub fn build(parsed: &ParsedNzb) -> DownloadQueue {
    let mut files: Vec<QueuedFile> = Vec::new();
    for seg in &parsed.segments {
        match files.last_mut() {
            Some(f) if f.name == seg.file_name => f.segments.push(QueuedSegment {
                message_id: seg.message_id.clone(),
                part: seg.part,
                bytes: seg.bytes,
            }),
            _ => files.push(QueuedFile {
                name: seg.file_name.clone(),
                segments: vec![QueuedSegment {
                    message_id: seg.message_id.clone(),
                    part: seg.part,
                    bytes: seg.bytes,
                }],
            }),
        }
    }
    DownloadQueue { files }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pesto::nzb::NzbMeta;
    use pesto::poster::PostedSegment;

    fn seg(name: &str, part: u32, total: u32, id: &str) -> PostedSegment {
        PostedSegment {
            file_name: name.into(),
            file_path: name.into(),
            subject_name: name.into(),
            file_size: 1000,
            part,
            total,
            message_id: id.into(),
            bytes: 500,
            from: "poster <p@x>".into(),
            date: (None, None),
            full_crc32: 0,
            server_idx: 0,
        }
    }

    #[test]
    fn groups_consecutive_segments_by_file() {
        let groups = vec!["alt.test".to_string()];
        let segments = vec![
            seg("a.bin", 1, 2, "<a1@x>"),
            seg("a.bin", 2, 2, "<a2@x>"),
            seg("b.bin", 1, 1, "<b1@x>"),
        ];
        let xml = pesto::nzb::generate(&groups, &segments, &NzbMeta::default());
        let parsed = pesto::nzb::parse(&xml).unwrap();

        let queue = build(&parsed);
        assert_eq!(queue.files.len(), 2);
        assert_eq!(queue.files[0].name, "a.bin");
        assert_eq!(queue.files[0].segments.len(), 2);
        assert_eq!(queue.files[1].name, "b.bin");
        assert_eq!(queue.files[1].segments.len(), 1);
    }
}
