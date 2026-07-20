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
/// grouped into one [`QueuedFile`]. Every file name is [`sanitize_file_name`]d
/// first — a `.nzb` is untrusted external input, and `QueuedFile::name`
/// eventually gets joined straight onto a destination directory
/// (`assemble::StreamingAssembly::new`).
pub fn build(parsed: &ParsedNzb) -> DownloadQueue {
    let mut files: Vec<QueuedFile> = Vec::new();
    for seg in &parsed.segments {
        let name = sanitize_file_name(&seg.file_name);
        match files.last_mut() {
            Some(f) if f.name == name => f.segments.push(QueuedSegment {
                message_id: seg.message_id.clone(),
                part: seg.part,
                bytes: seg.bytes,
            }),
            _ => files.push(QueuedFile {
                name,
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

/// Neutralize a `.nzb`-provided file name so it can never split into a
/// bogus nested directory or escape the destination directory once
/// [`assemble::StreamingAssembly::new`](crate::assemble::StreamingAssembly::new)
/// joins it onto `dest_dir`.
///
/// A `.nzb` is external, untrusted input — a malformed or adversarial one
/// can put anything in `<file name="...">`/the subject. Two concrete ways
/// that has bitten this exact join before it was sanitized:
/// - A literal `/` (or, on Windows, `\`) turns one path component into
///   several — e.g. `pesto::nzb::strip_part_suffix` used to leak `nyuu`'s
///   `[01/14] - ` subject counter straight into the "real" name, and the
///   `/` inside it silently created a `01` directory instead of staying
///   part of one file name.
/// - A name that's *exactly* `.`/`..` still means "this/parent directory"
///   to the OS even as a single path component, letting a crafted `.nzb`
///   point outside `dest_dir` entirely.
///
/// Replacing every separator with `_` closes the first case: a `/`-laden
/// name becomes one flat component instead of several. It does nothing for
/// a name that's *already* exactly `.`/`..` with no separator to replace,
/// so that case is checked separately.
fn sanitize_file_name(name: &str) -> String {
    let flat: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    if flat == "." || flat == ".." {
        format!("_{flat}")
    } else {
        flat
    }
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

    #[test]
    fn a_slash_in_the_file_name_is_flattened_not_left_to_split_into_a_directory() {
        let groups = vec!["alt.test".to_string()];
        let segments = vec![seg("[01/14] - \"real.mkv\"", 1, 1, "<a1@x>")];
        let xml = pesto::nzb::generate(&groups, &segments, &NzbMeta::default());
        let parsed = pesto::nzb::parse(&xml).unwrap();

        let queue = build(&parsed);
        assert_eq!(queue.files.len(), 1);
        assert!(!queue.files[0].name.contains('/'));
        assert_eq!(queue.files[0].name, "[01_14] - \"real.mkv\"");
    }

    #[test]
    fn sanitize_file_name_flattens_separators() {
        assert_eq!(sanitize_file_name("a/b\\c"), "a_b_c");
        assert_eq!(sanitize_file_name("movie.mkv"), "movie.mkv");
    }

    #[test]
    fn sanitize_file_name_neutralizes_dot_and_dotdot() {
        assert_eq!(sanitize_file_name("."), "_.");
        assert_eq!(sanitize_file_name(".."), "_..");
    }
}
