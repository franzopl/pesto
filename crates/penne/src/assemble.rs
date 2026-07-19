//! File assembly: turning decoded segment bodies back into whole files.
//!
//! [`StreamingAssembly`] writes each segment to its temp file the instant
//! it's decoded — not once every segment for the file has arrived — so a
//! large single-file release (a multi-GB video split into thousands of
//! segments, fetched in parallel) never needs to hold more than one
//! segment's decoded bytes in memory at a time. Segments are written at
//! their own byte offset (`DecodedPart::begin`) rather than appended in
//! fetch order, so the file assembles correctly regardless of which order
//! they actually arrived in — necessary once downloading is parallelized
//! (Phase 2's still-open N-connection item).
//!
//! The whole-file CRC-32 can't be accumulated by feeding bytes into a
//! running hasher as they're written, the way a single-pass batch assembler
//! would, since segments can arrive in any order. Instead, each segment's
//! *own* CRC-32 and length are kept (a few bytes per segment, not its
//! data), and [`pesto::yenc::crc32_combine`] folds them together in
//! ascending part order once every segment has landed — producing the
//! exact same result as hashing the concatenated bytes directly (that
//! identity is what `crc32_combine` is defined to guarantee, and is
//! verified empirically in `pesto::yenc`'s own tests), without ever
//! re-reading the file or holding its bytes beyond one segment at a time.
//!
//! Writes go to a temporary sibling path first, renamed into place only
//! once every segment has landed, so a killed download never leaves behind
//! a file that looks complete but isn't — and a file missing any segment
//! is never renamed into place at all, `Incomplete`'s temp file simply
//! discarded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pesto::yenc::{crc32, crc32_combine, DecodedPart};
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::progress::{ProgressEvent, ProgressSender};
use crate::queue::QueuedFile;

/// Result of assembling one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssembleOutcome {
    /// Every segment was present, written, and the whole-file CRC-32 (when
    /// one was known) matched. `actual_crc32` is that already-computed
    /// value — kept around (not just checked and discarded) so
    /// [`crate::health`]/`penne`'s PAR2 quick-check (`ROADMAP.md` Phase 16)
    /// can compare a file's integrity against PAR2 recovery data without
    /// paying to re-read and re-hash it from disk.
    Complete { actual_crc32: u32 },
    /// Every segment was present and written, but no whole-file CRC-32 was
    /// available to check against (some encoders never emit one on `=yend`
    /// for a multi-part file, though `pesto`'s own poster always does).
    /// `actual_crc32` is still the real computed value, for the same
    /// reason as [`Self::Complete`] — there was simply nothing to compare
    /// it against here.
    CompleteUnverified { actual_crc32: u32 },
    /// The file was written, but is suspect: either the whole-file CRC-32
    /// didn't match, or one or more parts failed their own CRC-32 (a
    /// structurally valid yEnc decode whose content doesn't match the
    /// checksum the sender claimed) — corruption in transit, not a parse
    /// failure. Left on disk anyway: a candidate for PAR2 repair (Phase 6)
    /// rather than something to discard.
    ChecksumMismatch {
        expected: Option<u32>,
        actual: u32,
        bad_parts: Vec<u32>,
    },
    /// One or more segments were never fetched/decoded; the file was not
    /// written at all — a partial file that looks complete is worse than no
    /// file.
    Incomplete { missing_parts: Vec<u32> },
}

/// Everything needed about one already-written segment to compute the
/// final `AssembleOutcome`, once every segment has landed — deliberately
/// *not* the segment's own bytes, which are written to disk and dropped
/// immediately.
struct PartMeta {
    /// CRC-32 of this part's actual decoded bytes (not the sender's
    /// *claimed* checksum — computed fresh here regardless of whether the
    /// claim matched, matching what a single-pass hasher over the real
    /// bytes would have produced).
    crc32: u32,
    len: u64,
    /// Whether this part's own claimed checksum (`DecodedPart::crc_matches`)
    /// was honest.
    matches: bool,
    /// This part's `=yend crc32=` field, if it carried one.
    file_crc32: Option<u32>,
}

/// One file's assembly in progress: an open temp-file handle (created
/// lazily, on the first successfully written segment) plus enough
/// per-segment metadata to decide [`AssembleOutcome`] once every segment
/// has arrived, without ever holding more than one segment's decoded bytes
/// in memory. Construct with [`StreamingAssembly::new`], feed it segments
/// via [`StreamingAssembly::write_segment`] as they're decoded (in any
/// order), and finish with [`StreamingAssembly::finish`] once the file's
/// segment count is known to be exhausted.
pub struct StreamingAssembly {
    name: String,
    tmp_path: PathBuf,
    final_path: PathBuf,
    file: Option<File>,
    /// Where the file cursor already is, so [`Self::write_segment`] only
    /// issues a `seek()` when it would actually move it — see that
    /// method's doc comment.
    cursor: u64,
    parts: HashMap<u32, PartMeta>,
}

impl StreamingAssembly {
    /// Start assembling `file` into `dest_dir.join(&file.name)` — the real
    /// filename from the `.nzb`, never obfuscated, unlike a yEnc `name=`
    /// field (see [`pesto::nzb`]'s module docs). No I/O happens here; the
    /// temp file is only created on the first successful
    /// [`Self::write_segment`] call, so a file that never gets a single
    /// segment written never touches disk at all.
    pub fn new(file: &QueuedFile, dest_dir: &Path) -> Self {
        let final_path = dest_dir.join(&file.name);
        let tmp_path = tmp_path_for(&final_path);
        StreamingAssembly {
            name: file.name.clone(),
            tmp_path,
            final_path,
            file: None,
            cursor: 0,
            parts: HashMap::new(),
        }
    }

    /// Write one decoded segment to the temp file at its own byte offset
    /// (`part.begin`), and record just enough about it (CRC-32, length —
    /// not the data itself) to fold into the whole-file verdict once
    /// [`Self::finish`] is called.
    ///
    /// `queue_part` is the segment's part number *as the `.nzb`/queue
    /// knows it* (`QueuedSegment::part`) — deliberately not
    /// `part.part` (the yEnc article's own claimed `=ypart number=`),
    /// which is not guaranteed to agree with it: nothing stops a sender
    /// (or a test fixture — this is exactly how a real bug here was first
    /// caught) from mislabeling that field. The queue's own numbering,
    /// derived purely from `.nzb` segment order, is what
    /// [`Self::finish`]'s `expected_parts` and every other part of this
    /// codebase already treats as authoritative; using the article's own
    /// claim as this map's key instead would silently collapse multiple
    /// distinct segments together if their claims happened to collide.
    ///
    /// Tracks where the file cursor already is, so a `seek()` is only
    /// issued when it would actually move it. `file.segments` is sorted by
    /// part (`crate::queue::build`), so consecutive parts' byte ranges are
    /// contiguous in the overwhelming common case — the cursor left by one
    /// `write_all` already sits exactly where the next part needs to
    /// start. A `seek()` per segment regardless was previously measured to
    /// be ~3x slower than this on real disks (not on tmpfs/SSD, where the
    /// difference vanishes into noise): each `tokio::fs` call dispatches
    /// through a blocking-thread-pool round trip, and a redundant explicit
    /// seek between two otherwise sequential writes was enough to defeat
    /// filesystem-level write-coalescing on at least one real-world setup
    /// (btrfs, nearly full). Correctness is unaffected either way — an
    /// out-of-order part (the common case here, unlike the single-pass
    /// batch assembler this replaced) still seeks exactly as needed.
    pub async fn write_segment(&mut self, queue_part: u32, part: &DecodedPart) -> Result<()> {
        if self.file.is_none() {
            if let Some(parent) = self.final_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            self.file = Some(
                File::create(&self.tmp_path)
                    .await
                    .with_context(|| format!("creating {}", self.tmp_path.display()))?,
            );
            self.cursor = 0;
        }
        let file = self.file.as_mut().expect("just ensured Some above");

        let offset = part.begin.saturating_sub(1);
        if self.cursor != offset {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .with_context(|| format!("seeking in {}", self.tmp_path.display()))?;
            self.cursor = offset;
        }
        file.write_all(&part.data)
            .await
            .with_context(|| format!("writing to {}", self.tmp_path.display()))?;
        self.cursor += part.data.len() as u64;

        self.parts.insert(
            queue_part,
            PartMeta {
                crc32: crc32(&part.data),
                len: part.data.len() as u64,
                matches: part.crc_matches(),
                file_crc32: part.file_crc32,
            },
        );
        Ok(())
    }

    /// Decide the final [`AssembleOutcome`] now that `expected_parts` (every
    /// part number the `.nzb` lists for this file, any order) is known to
    /// be exhausted — every part either landed via [`Self::write_segment`]
    /// or definitively never will.
    ///
    /// Any part in `expected_parts` that was never written makes the whole
    /// file [`AssembleOutcome::Incomplete`]: the temp file (if one was ever
    /// created) is discarded, never renamed into place — a partial file
    /// that looks complete is worse than no file. Otherwise, the temp file
    /// is renamed into place and the whole-file CRC-32 is folded from every
    /// part's own CRC-32 via [`crc32_combine`] in ascending part order
    /// (matching what a single-pass hasher over the concatenated bytes
    /// would have produced — see the module doc comment).
    pub async fn finish(
        mut self,
        expected_parts: &[u32],
        progress: Option<&ProgressSender>,
    ) -> Result<AssembleOutcome> {
        let mut missing_parts: Vec<u32> = expected_parts
            .iter()
            .filter(|p| !self.parts.contains_key(p))
            .copied()
            .collect();
        missing_parts.sort_unstable();
        if !missing_parts.is_empty() {
            if let Some(file) = self.file.take() {
                drop(file);
                let _ = tokio::fs::remove_file(&self.tmp_path).await;
            }
            return Ok(AssembleOutcome::Incomplete { missing_parts });
        }

        let mut sorted_parts = expected_parts.to_vec();
        sorted_parts.sort_unstable();

        let mut actual = 0u32;
        let mut bad_parts = Vec::new();
        let mut expected_file_crc32 = None;
        for part_num in &sorted_parts {
            let meta = &self.parts[part_num];
            actual = crc32_combine(actual, meta.crc32, meta.len);
            if !meta.matches {
                bad_parts.push(*part_num);
            }
            if expected_file_crc32.is_none() {
                expected_file_crc32 = meta.file_crc32;
            }
        }

        if let Some(mut file) = self.file.take() {
            file.flush()
                .await
                .with_context(|| format!("flushing {}", self.tmp_path.display()))?;
        }
        tokio::fs::rename(&self.tmp_path, &self.final_path)
            .await
            .with_context(|| {
                format!(
                    "renaming {} to {}",
                    self.tmp_path.display(),
                    self.final_path.display()
                )
            })?;

        if let Some(tx) = progress {
            let _ = tx.send(ProgressEvent::FileAssembled {
                file_name: self.name.clone(),
            });
        }

        let outcome = match expected_file_crc32 {
            Some(expected) if expected != actual || !bad_parts.is_empty() => {
                AssembleOutcome::ChecksumMismatch {
                    expected: Some(expected),
                    actual,
                    bad_parts,
                }
            }
            Some(_) => AssembleOutcome::Complete {
                actual_crc32: actual,
            },
            None if !bad_parts.is_empty() => AssembleOutcome::ChecksumMismatch {
                expected: None,
                actual,
                bad_parts,
            },
            None => AssembleOutcome::CompleteUnverified {
                actual_crc32: actual,
            },
        };
        Ok(outcome)
    }
}

/// Temporary sibling path for `final_path`, used while assembly is in
/// progress. Sibling (not a separate temp directory) so the final rename is
/// a same-filesystem, same-directory move.
fn tmp_path_for(final_path: &Path) -> PathBuf {
    let mut name = final_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".penne-part");
    final_path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::QueuedSegment;
    use pesto::yenc::{decode_part, encode_part, PartSpec};

    fn decoded_part(
        name: &str,
        number: u32,
        total: u32,
        offset: u64,
        data: &[u8],
        file_crc32: Option<u32>,
    ) -> DecodedPart {
        let encoded = encode_part(
            name,
            10_000,
            PartSpec {
                number,
                total,
                offset,
            },
            data,
            128,
            file_crc32,
        );
        decode_part(&encoded.body).unwrap()
    }

    fn queued_file(name: &str, parts: &[u32]) -> QueuedFile {
        QueuedFile {
            name: name.to_string(),
            segments: parts
                .iter()
                .map(|&p| QueuedSegment {
                    message_id: format!("id{p}@test"),
                    part: p,
                    bytes: 0,
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn writes_segments_at_their_offset_and_verifies_file_crc() {
        let dir = tempfile::tempdir().unwrap();
        let part1_data = b"hello ".to_vec();
        let part2_data = b"world!".to_vec();
        let whole_crc = pesto::yenc::crc32(&[part1_data.clone(), part2_data.clone()].concat());

        let file = queued_file("movie.bin", &[1, 2]);
        let mut assembly = StreamingAssembly::new(&file, dir.path());
        assembly
            .write_segment(
                1,
                &decoded_part("movie.bin", 1, 2, 0, &part1_data, Some(whole_crc)),
            )
            .await
            .unwrap();
        assembly
            .write_segment(
                2,
                &decoded_part(
                    "movie.bin",
                    2,
                    2,
                    part1_data.len() as u64,
                    &part2_data,
                    Some(whole_crc),
                ),
            )
            .await
            .unwrap();
        let outcome = assembly.finish(&[1, 2], None).await.unwrap();
        assert_eq!(
            outcome,
            AssembleOutcome::Complete {
                actual_crc32: whole_crc
            }
        );

        let written = tokio::fs::read(dir.path().join("movie.bin")).await.unwrap();
        assert_eq!(written, b"hello world!".to_vec());
        // Temp sibling must not survive a successful assemble.
        assert!(!dir.path().join("movie.bin.penne-part").exists());
    }

    #[tokio::test]
    async fn writing_segments_out_of_order_still_assembles_correctly() {
        // Write part 2 before part 1 — proves the file layout comes from
        // each part's own `begin` offset, not arrival order. This is also
        // the *common* case now (segments genuinely arrive out of order
        // across parallel connections), not just a defensive edge case.
        let dir = tempfile::tempdir().unwrap();
        let part1_data = b"AAAA".to_vec();
        let part2_data = b"BBBB".to_vec();

        let file = queued_file("f.bin", &[1, 2]);
        let mut assembly = StreamingAssembly::new(&file, dir.path());
        assembly
            .write_segment(
                2,
                &decoded_part("f.bin", 2, 2, part1_data.len() as u64, &part2_data, None),
            )
            .await
            .unwrap();
        assembly
            .write_segment(1, &decoded_part("f.bin", 1, 2, 0, &part1_data, None))
            .await
            .unwrap();
        assembly.finish(&[1, 2], None).await.unwrap();

        let written = tokio::fs::read(dir.path().join("f.bin")).await.unwrap();
        assert_eq!(written, b"AAAABBBB".to_vec());
    }

    #[tokio::test]
    async fn keys_bookkeeping_by_the_queue_part_number_not_the_articles_own_claim() {
        // Regression test: a real bug caught this exact scenario. Every
        // segment here decodes with the *same* claimed `=ypart number=1`
        // (simulating a sloppy/misbehaving poster, or simply a test
        // fixture that never bothered setting it correctly — both happen
        // in practice), yet each is a genuinely distinct segment of the
        // file per the queue's own numbering. `write_segment` must track
        // them by the `queue_part` argument, not `DecodedPart::part` —
        // otherwise every write after the first collapses into the same
        // bookkeeping slot, and `finish` wrongly reports every part after
        // the first as missing.
        let dir = tempfile::tempdir().unwrap();
        let part1_data = b"AAAA".to_vec();
        let part2_data = b"BBBB".to_vec();
        let part3_data = b"CCCC".to_vec();

        let file = queued_file("f.bin", &[1, 2, 3]);
        let mut assembly = StreamingAssembly::new(&file, dir.path());
        // `number: 1` on every call, regardless of which queue part this
        // actually is — `total: 3` (correctly reflecting a 3-part message,
        // so none of these individually satisfies yEnc's own
        // number-equals-total convention for emitting a whole-file
        // `=yend crc32=`) keeps this test isolated to the bug under test
        // rather than also exercising whole-file-CRC handling, already
        // covered elsewhere.
        assembly
            .write_segment(1, &decoded_part("f.bin", 1, 3, 0, &part1_data, None))
            .await
            .unwrap();
        assembly
            .write_segment(
                2,
                &decoded_part("f.bin", 1, 3, part1_data.len() as u64, &part2_data, None),
            )
            .await
            .unwrap();
        assembly
            .write_segment(
                3,
                &decoded_part(
                    "f.bin",
                    1,
                    3,
                    (part1_data.len() + part2_data.len()) as u64,
                    &part3_data,
                    None,
                ),
            )
            .await
            .unwrap();

        let outcome = assembly.finish(&[1, 2, 3], None).await.unwrap();
        assert!(
            matches!(outcome, AssembleOutcome::CompleteUnverified { .. }),
            "expected all three parts to be recognized as present, got {outcome:?}"
        );
        let written = tokio::fs::read(dir.path().join("f.bin")).await.unwrap();
        assert_eq!(written, b"AAAABBBBCCCC".to_vec());
    }

    #[tokio::test]
    async fn missing_segment_returns_incomplete_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = queued_file("movie.bin", &[1, 2]);
        let mut assembly = StreamingAssembly::new(&file, dir.path());
        assembly
            .write_segment(1, &decoded_part("movie.bin", 1, 2, 0, b"hello ", None))
            .await
            .unwrap();
        // Part 2 never written.

        let outcome = assembly.finish(&[1, 2], None).await.unwrap();
        assert_eq!(
            outcome,
            AssembleOutcome::Incomplete {
                missing_parts: vec![2]
            }
        );
        assert!(!dir.path().join("movie.bin").exists());
        assert!(!dir.path().join("movie.bin.penne-part").exists());
    }

    #[tokio::test]
    async fn entirely_unwritten_file_never_touches_disk() {
        let dir = tempfile::tempdir().unwrap();
        let file = queued_file("ghost.bin", &[1]);
        let assembly = StreamingAssembly::new(&file, dir.path());
        // No write_segment call at all.
        let outcome = assembly.finish(&[1], None).await.unwrap();
        assert_eq!(
            outcome,
            AssembleOutcome::Incomplete {
                missing_parts: vec![1]
            }
        );
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "no file (final or temp) should have been created"
        );
    }

    #[tokio::test]
    async fn single_part_file_with_no_yend_crc_is_unverified() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"just one part".to_vec();
        let mut part = decoded_part("f.bin", 1, 1, 0, &data, None);
        // Simulate a sender that never included a checksum at all.
        part.part_crc32 = None;
        part.file_crc32 = None;

        let file = queued_file("f.bin", &[1]);
        let mut assembly = StreamingAssembly::new(&file, dir.path());
        assembly.write_segment(1, &part).await.unwrap();
        let outcome = assembly.finish(&[1], None).await.unwrap();
        assert_eq!(
            outcome,
            AssembleOutcome::CompleteUnverified {
                actual_crc32: pesto::yenc::crc32(&data)
            }
        );
    }

    #[tokio::test]
    async fn corrupted_part_is_reported_but_file_is_still_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut part1 = decoded_part("f.bin", 1, 2, 0, b"good", None);
        // Force a checksum that cannot match this part's actual content.
        part1.part_crc32 = Some(0xDEAD_BEEF);

        let file = queued_file("f.bin", &[1, 2]);
        let mut assembly = StreamingAssembly::new(&file, dir.path());
        assembly.write_segment(1, &part1).await.unwrap();
        assembly
            .write_segment(2, &decoded_part("f.bin", 2, 2, 4, b"data", None))
            .await
            .unwrap();

        let outcome = assembly.finish(&[1, 2], None).await.unwrap();
        match outcome {
            AssembleOutcome::ChecksumMismatch { bad_parts, .. } => {
                assert_eq!(bad_parts, vec![1]);
            }
            other => panic!("expected ChecksumMismatch, got {other:?}"),
        }
        // Still written to disk — a candidate for repair, not discarded.
        assert!(dir.path().join("f.bin").exists());
    }

    #[tokio::test]
    async fn whole_file_crc32_matches_a_single_pass_hash_regardless_of_write_order() {
        // The real correctness claim of the crc32_combine approach: folding
        // each part's own CRC-32 in ascending order must equal hashing the
        // concatenated bytes directly, even when parts were *written* out
        // of order.
        let parts_data: Vec<Vec<u8>> = (0..5)
            .map(|i| (0..37u32).map(|b| ((b + i) * 7) as u8).collect())
            .collect();
        let whole: Vec<u8> = parts_data.concat();
        let expected = pesto::yenc::crc32(&whole);

        let file = queued_file("f.bin", &[1, 2, 3, 4, 5]);
        let dir = tempfile::tempdir().unwrap();
        let mut assembly = StreamingAssembly::new(&file, dir.path());

        // Write in a scrambled order: 3, 1, 5, 2, 4.
        let mut offset = 0u64;
        let offsets: Vec<u64> = parts_data
            .iter()
            .map(|d| {
                let o = offset;
                offset += d.len() as u64;
                o
            })
            .collect();
        for &i in &[2usize, 0, 4, 1, 3] {
            assembly
                .write_segment(
                    i as u32 + 1,
                    &decoded_part("f.bin", i as u32 + 1, 5, offsets[i], &parts_data[i], None),
                )
                .await
                .unwrap();
        }

        let outcome = assembly.finish(&[1, 2, 3, 4, 5], None).await.unwrap();
        assert_eq!(
            outcome,
            AssembleOutcome::CompleteUnverified {
                actual_crc32: expected
            }
        );
        let written = tokio::fs::read(dir.path().join("f.bin")).await.unwrap();
        assert_eq!(written, whole);
    }
}
