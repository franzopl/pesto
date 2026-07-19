//! Free-disk-space guard, checked once before a download starts.
//!
//! `penne` previously had no free-space check anywhere: a multi-GiB release
//! could fail partway through [`crate::assemble::assemble`] purely from
//! running out of disk, indistinguishable at a glance from a real bug. This
//! mirrors `nzbget`'s `DiskSpace` option — fail clearly up front instead.

use std::path::Path;

use anyhow::{Context, Result};

use crate::queue::DownloadQueue;

/// Sum of every queued segment's wire (yEnc-encoded) byte count — a slight
/// overestimate of the decoded output size (yEnc encoding always expands
/// the data a little), which is the safe direction to round for a
/// pre-flight check: it can only ask for a bit more headroom than strictly
/// needed, never less.
pub fn required_bytes(queue: &DownloadQueue) -> u64 {
    queue
        .files
        .iter()
        .flat_map(|f| &f.segments)
        .map(|s| s.bytes)
        .sum()
}

/// Result of comparing the queue's size against free space on the
/// destination filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceCheck {
    pub required: u64,
    pub available: u64,
}

impl SpaceCheck {
    pub fn is_enough(&self) -> bool {
        self.available >= self.required
    }
}

/// Check free space on the filesystem holding `dest_dir` against
/// `required` bytes. Creates `dest_dir` if it doesn't exist yet (it will be
/// needed imminently regardless — [`crate::assemble::assemble`] would
/// create it lazily per file otherwise), since `fs4::available_space`
/// requires an existing path to stat.
pub fn check(dest_dir: &Path, required: u64) -> Result<SpaceCheck> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating {}", dest_dir.display()))?;
    let available = fs4::available_space(dest_dir)
        .with_context(|| format!("checking free space on {}", dest_dir.display()))?;
    Ok(SpaceCheck {
        required,
        available,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{QueuedFile, QueuedSegment};

    fn queue_with_bytes(sizes: &[u64]) -> DownloadQueue {
        DownloadQueue {
            files: vec![QueuedFile {
                name: "f.bin".to_string(),
                segments: sizes
                    .iter()
                    .enumerate()
                    .map(|(i, &bytes)| QueuedSegment {
                        message_id: format!("id{i}@test"),
                        part: i as u32 + 1,
                        bytes,
                    })
                    .collect(),
            }],
        }
    }

    #[test]
    fn required_bytes_sums_every_segment_across_every_file() {
        let mut queue = queue_with_bytes(&[100, 200]);
        queue.files.push(QueuedFile {
            name: "g.bin".to_string(),
            segments: vec![QueuedSegment {
                message_id: "id-g@test".to_string(),
                part: 1,
                bytes: 50,
            }],
        });
        assert_eq!(required_bytes(&queue), 350);
    }

    #[test]
    fn required_bytes_of_empty_queue_is_zero() {
        assert_eq!(required_bytes(&DownloadQueue::default()), 0);
    }

    #[test]
    fn space_check_reports_enough_for_a_tiny_requirement() {
        let dir = tempfile::tempdir().unwrap();
        let check = check(dir.path(), 1).unwrap();
        assert!(check.is_enough());
    }

    #[test]
    fn space_check_reports_not_enough_for_an_impossible_requirement() {
        let dir = tempfile::tempdir().unwrap();
        let check = check(dir.path(), u64::MAX).unwrap();
        assert!(!check.is_enough());
    }

    #[test]
    fn check_creates_a_destination_directory_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("not-yet-created");
        assert!(!dest.exists());
        check(&dest, 1).unwrap();
        assert!(dest.exists());
    }
}
