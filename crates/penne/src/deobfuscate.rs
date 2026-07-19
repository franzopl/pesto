//! Recovering real file names from an obfuscated Usenet release.
//!
//! Obfuscated posts (common for scene/P2P releases, deliberately so
//! automated filtering can't act on the real filename) leave `penne` with
//! nothing but hash-like names once assembled — [`pesto::nzb::parse`]
//! derives `file_name` from the `.nzb` subject when no `name` attribute is
//! present, and a fully obfuscated subject has no real name in it either.
//! `find_par2_index` ([`crate::repair`]) and `find_extractable`
//! ([`crate::extract`]) both identify files by *extension*, so an obfuscated
//! release's PAR2 set and archive volumes are otherwise invisible to them.
//!
//! This module runs once, after [`crate::assemble`] and before
//! [`crate::repair`]/[`crate::extract`], and renames files on disk so those
//! two stages need no changes at all:
//!
//! 1. **Content-sniff PAR2** ([`pesto::par2::packet_reader::read_packets`],
//!    already public) regardless of extension, and tag every match with a
//!    `.par2` suffix — `find_par2_index`/`RecoverySet::load` then find the
//!    whole set exactly as they already do for a non-obfuscated release.
//! 2. **Match every other file** against the loaded recovery set's
//!    [`FileEntry`] list by `(length, first-16KiB MD5)` — the same signal
//!    SABnzbd/NZBGet use for this — and rename matches to their real name.
//!    This is the high-confidence path: [`RenameReason::Par2Recovered`].
//! 3. **Guess** whatever's left uncovered by PAR2 (or when there's no PAR2
//!    at all): sniff for a RAR/7z/Zip signature and rename using `.nzb`
//!    queue order as a best-effort volume sequence
//!    ([`RenameReason::Guessed`]) — the poster's own splitting tool almost
//!    always lists volumes in that order, but this is inherently
//!    unverifiable without PAR2 coverage.
//!
//! **Known limitations**, not solved here: only the first PAR2 recovery set
//! found is used if a directory somehow holds more than one (matching
//! `find_par2_index`'s own pre-existing single-set assumption); the guess
//! pass can't tell two unrelated archive sets of the same kind apart; and
//! multi-volume ZIP isn't guessed at all (`crate::extract` has no
//! multi-volume ZIP support to hand a guessed sequence to in the first
//! place).

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pesto::par2::packet;
use pesto::par2::packet_reader::read_packets;
use pesto::par2::recovery_set::{FileEntry, RecoverySet};
use tracing::warn;

use crate::assemble::AssembleOutcome;
use crate::extract::{self, ArchiveKind};
use crate::queue::DownloadQueue;

/// How much of a file to read when sniffing for PAR2 packets. Main/Creator/
/// FileDesc/IFSC packets are always small and written before any bulk
/// `RecvSlic` data by every PAR2 encoder (including `parmesan`'s own — see
/// `crates/parmesan/src/packet.rs`), so one always fits in this prefix even
/// for a multi-megabyte recovery volume.
const PAR2_SNIFF_PREFIX: usize = 64 * 1024;
/// Matches the fixed 16 KiB PAR2 uses for its `md5_16k` File Description
/// field.
const MD5_16K: usize = 16 * 1024;
/// How much of a file to read when sniffing for an archive signature —
/// every format `crate::extract` recognises has its magic in the first few
/// bytes.
const ARCHIVE_SNIFF_PREFIX: usize = 4096;

/// Why a file was renamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameReason {
    /// Content-sniffed as PAR2 packets; tagged with a `.par2` suffix so the
    /// unmodified `find_par2_index`/`RecoverySet::load` find it normally.
    Par2Volume,
    /// Matched a PAR2 File Description packet by `(length, first-16KiB
    /// MD5)` — high confidence, this is the file's true original name.
    Par2Recovered,
    /// No PAR2 coverage; renamed from a sniffed archive magic-byte
    /// signature plus its position in the `.nzb`'s file order. Best-effort.
    Guessed,
}

/// One rename this pass made.
#[derive(Debug, Clone)]
pub struct Rename {
    pub old_name: String,
    pub new_name: String,
    pub reason: RenameReason,
}

/// Every rename [`run`] made, in the order they happened.
#[derive(Debug, Clone, Default)]
pub struct RenameReport {
    pub renames: Vec<Rename>,
}

/// One assembled file, as a rename candidate.
struct Candidate {
    name: String,
    path: PathBuf,
}

/// Recover real file names for the release [`crate::download::download_queue`]
/// just wrote under `dest_dir`. `synthetic_base` (typically the `.nzb`
/// file's own stem — often a real, human-readable name even when the
/// release's *internal* subjects are obfuscated) seeds names for files the
/// guess pass renames.
///
/// Runs on `tokio::task::spawn_blocking`, mirroring
/// [`crate::repair::verify_and_repair`]'s wrapper: directory scans, file
/// reads and hashing are blocking work, not async I/O.
pub async fn run(
    dest_dir: &Path,
    queue: &DownloadQueue,
    assembled: &HashMap<String, AssembleOutcome>,
    synthetic_base: &str,
) -> Result<RenameReport> {
    // Only files assemble actually wrote bytes for are rename candidates —
    // `Incomplete` means nothing landed on disk.
    let candidates: Vec<Candidate> = queue
        .files
        .iter()
        .filter(|f| {
            matches!(
                assembled.get(&f.name),
                Some(AssembleOutcome::Complete)
                    | Some(AssembleOutcome::CompleteUnverified)
                    | Some(AssembleOutcome::ChecksumMismatch { .. })
            )
        })
        .map(|f| Candidate {
            name: f.name.clone(),
            path: dest_dir.join(&f.name),
        })
        .collect();

    let dest_dir = dest_dir.to_path_buf();
    let synthetic_base = synthetic_base.to_string();
    tokio::task::spawn_blocking(move || run_blocking(&dest_dir, candidates, &synthetic_base))
        .await
        .context("deobfuscation task panicked")?
}

fn run_blocking(
    dest_dir: &Path,
    candidates: Vec<Candidate>,
    synthetic_base: &str,
) -> Result<RenameReport> {
    let mut report = RenameReport::default();

    let (par2_paths, mut rest) = tag_par2_content(dest_dir, candidates, &mut report);

    let recovery_files: Vec<FileEntry> = match par2_paths.first() {
        Some(index) => match RecoverySet::load(index) {
            Ok(set) => set.files,
            Err(e) => {
                warn!(
                    error = %e,
                    path = %index.display(),
                    "content looked like PAR2 but failed to load as a recovery set; skipping PAR2-based recovery"
                );
                Vec::new()
            }
        },
        None => Vec::new(),
    };

    match_against_recovery_set(dest_dir, &recovery_files, &mut rest, &mut report);
    guess_remaining(dest_dir, rest, synthetic_base, &mut report);

    Ok(report)
}

/// Partition `candidates` into PAR2-content files (renamed to end in
/// `.par2` when they didn't already) and everything else.
fn tag_par2_content(
    dest_dir: &Path,
    candidates: Vec<Candidate>,
    report: &mut RenameReport,
) -> (Vec<PathBuf>, Vec<Candidate>) {
    let mut par2_paths = Vec::new();
    let mut rest = Vec::new();

    for c in candidates {
        let looks_like_par2 = read_prefix(&c.path, PAR2_SNIFF_PREFIX)
            .map(|prefix| !read_packets(&prefix).is_empty())
            .unwrap_or(false);
        if !looks_like_par2 {
            rest.push(c);
            continue;
        }
        if c.name.to_ascii_lowercase().ends_with(".par2") {
            par2_paths.push(c.path);
            continue;
        }
        let new_name = format!("{}.par2", c.name);
        let new_path = dest_dir.join(&new_name);
        match fs::rename(&c.path, &new_path) {
            Ok(()) => {
                report.renames.push(Rename {
                    old_name: c.name,
                    new_name,
                    reason: RenameReason::Par2Volume,
                });
                par2_paths.push(new_path);
            }
            Err(e) => {
                warn!(error = %e, path = %c.path.display(), "failed to tag PAR2-content file with a .par2 suffix");
                par2_paths.push(c.path);
            }
        }
    }

    (par2_paths, rest)
}

/// Match every remaining candidate against the recovery set's file list by
/// `(length, first-16KiB MD5)`, renaming matches to their real name.
fn match_against_recovery_set(
    dest_dir: &Path,
    recovery_files: &[FileEntry],
    rest: &mut Vec<Candidate>,
    report: &mut RenameReport,
) {
    for entry in recovery_files {
        let target = dest_dir.join(&entry.name);
        if target.exists() {
            // Already correctly named, or a genuine name collision either
            // way — never overwrite.
            continue;
        }
        let Some(idx) = rest.iter().position(|c| matches_entry(c, entry)) else {
            continue;
        };
        let path = rest[idx].path.clone();
        let name = rest[idx].name.clone();
        match fs::rename(&path, &target) {
            Ok(()) => {
                report.renames.push(Rename {
                    old_name: name,
                    new_name: entry.name.clone(),
                    reason: RenameReason::Par2Recovered,
                });
                rest.remove(idx);
            }
            Err(e) => {
                warn!(error = %e, path = %path.display(), "PAR2 identified this file's real name but renaming it failed");
            }
        }
    }
}

fn matches_entry(c: &Candidate, entry: &FileEntry) -> bool {
    let Ok(meta) = fs::metadata(&c.path) else {
        return false;
    };
    if meta.len() != entry.length {
        return false;
    }
    let Ok(prefix) = read_prefix(&c.path, MD5_16K) else {
        return false;
    };
    packet::md5(&prefix) == entry.md5_16k
}

/// Best-effort rename of whatever PAR2 didn't cover: sniff for an archive
/// signature and number volumes in `.nzb` queue order.
fn guess_remaining(
    dest_dir: &Path,
    rest: Vec<Candidate>,
    synthetic_base: &str,
    report: &mut RenameReport,
) {
    let mut rar = Vec::new();
    let mut seven_zip = Vec::new();
    let mut zip = Vec::new();

    for c in rest {
        let Ok(prefix) = read_prefix(&c.path, ARCHIVE_SNIFF_PREFIX) else {
            continue;
        };
        match extract::sniff(&prefix) {
            Some(ArchiveKind::Rar) => rar.push(c),
            Some(ArchiveKind::SevenZip) => seven_zip.push(c),
            Some(ArchiveKind::Zip) => zip.push(c),
            // Not a recognised archive — no safe guess to make; leave it
            // under its current (obfuscated/subject-derived) name.
            None => {}
        }
    }

    let total = rar.len();
    for (i, c) in rar.into_iter().enumerate() {
        let new_name = if total == 1 {
            format!("{synthetic_base}.rar")
        } else if total >= 100 {
            format!("{synthetic_base}.part{:03}.rar", i + 1)
        } else {
            format!("{synthetic_base}.part{:02}.rar", i + 1)
        };
        try_rename(dest_dir, &c, &new_name, RenameReason::Guessed, report);
    }

    let total = seven_zip.len();
    for (i, c) in seven_zip.into_iter().enumerate() {
        // `classify()` (crate::extract) requires the volume-number suffix
        // to be at least 3 digits — not just cosmetic zero-padding.
        let new_name = if total == 1 {
            format!("{synthetic_base}.7z")
        } else {
            format!("{synthetic_base}.7z.{:03}", i + 1)
        };
        try_rename(dest_dir, &c, &new_name, RenameReason::Guessed, report);
    }

    // `crate::extract` has no multi-volume ZIP support today, so only the
    // first (queue order) gets a confident guess — the rest are left
    // alone rather than invented into a naming scheme nothing downstream
    // can consume.
    if let Some(first) = zip.into_iter().next() {
        try_rename(
            dest_dir,
            &first,
            &format!("{synthetic_base}.zip"),
            RenameReason::Guessed,
            report,
        );
    }
}

fn try_rename(
    dest_dir: &Path,
    c: &Candidate,
    new_name: &str,
    reason: RenameReason,
    report: &mut RenameReport,
) {
    let target = dest_dir.join(new_name);
    if target.exists() {
        return;
    }
    match fs::rename(&c.path, &target) {
        Ok(()) => report.renames.push(Rename {
            old_name: c.name.clone(),
            new_name: new_name.to_string(),
            reason,
        }),
        Err(e) => warn!(error = %e, path = %c.path.display(), "guessed rename failed"),
    }
}

/// Read up to `max` bytes from the start of `path`.
fn read_prefix(path: &Path, max: usize) -> Result<Vec<u8>> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = vec![0u8; max];
    let mut total = 0;
    loop {
        let n = f.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
        if total == buf.len() {
            break;
        }
    }
    buf.truncate(total);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pesto::par2::packet as p2packet;

    fn queue_with(names: &[&str]) -> DownloadQueue {
        use crate::queue::{QueuedFile, QueuedSegment};
        DownloadQueue {
            files: names
                .iter()
                .map(|n| QueuedFile {
                    name: n.to_string(),
                    segments: vec![QueuedSegment {
                        message_id: format!("<{n}@x>"),
                        part: 1,
                        bytes: 10,
                    }],
                })
                .collect(),
        }
    }

    fn complete_map(names: &[&str]) -> HashMap<String, AssembleOutcome> {
        names
            .iter()
            .map(|n| (n.to_string(), AssembleOutcome::Complete))
            .collect()
    }

    #[test]
    fn matches_entry_checks_length_and_16k_hash() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"hello world, this is test data".to_vec();
        let path = dir.path().join("abc123");
        std::fs::write(&path, &data).unwrap();

        let c = Candidate {
            name: "abc123".into(),
            path: path.clone(),
        };
        let entry = FileEntry {
            file_id: [0; 16],
            name: "real.bin".into(),
            length: data.len() as u64,
            md5_full: p2packet::md5(&data),
            md5_16k: p2packet::md5(&data),
            slice_checksums: Vec::new(),
        };
        assert!(matches_entry(&c, &entry));

        let wrong_length = FileEntry {
            length: data.len() as u64 + 1,
            ..entry.clone()
        };
        assert!(!matches_entry(&c, &wrong_length));

        let wrong_hash = FileEntry {
            md5_16k: p2packet::md5(b"different content"),
            ..entry
        };
        assert!(!matches_entry(&c, &wrong_hash));
    }

    #[tokio::test]
    async fn par2_content_is_tagged_regardless_of_original_name() {
        let dir = tempfile::tempdir().unwrap();
        // A minimal but genuinely valid PAR2 packet (a Creator packet is
        // enough for `read_packets` to recognise the file as PAR2 content
        // — no need for a full loadable recovery set just to test sniffing).
        let body = p2packet::creator_body("test");
        let packet_bytes = p2packet::serialize_packet(&[0u8; 16], &p2packet::TYPE_CREATOR, &body);
        std::fs::write(dir.path().join("f7e2a91b"), &packet_bytes).unwrap();
        std::fs::write(dir.path().join("plain.txt"), b"not par2 at all").unwrap();

        let queue = queue_with(&["f7e2a91b", "plain.txt"]);
        let assembled = complete_map(&["f7e2a91b", "plain.txt"]);

        let report = run(dir.path(), &queue, &assembled, "release")
            .await
            .unwrap();

        assert_eq!(report.renames.len(), 1);
        assert_eq!(report.renames[0].old_name, "f7e2a91b");
        assert_eq!(report.renames[0].new_name, "f7e2a91b.par2");
        assert_eq!(report.renames[0].reason, RenameReason::Par2Volume);
        assert!(dir.path().join("f7e2a91b.par2").exists());
        assert!(dir.path().join("plain.txt").exists());
    }

    #[tokio::test]
    async fn guesses_rar_volume_order_from_queue_order_when_no_par2_present() {
        let dir = tempfile::tempdir().unwrap();
        let rar_magic = b"Rar!\x1a\x07\x00rest of a fake volume";
        std::fs::write(dir.path().join("zzz1"), rar_magic).unwrap();
        std::fs::write(dir.path().join("zzz2"), rar_magic).unwrap();

        let queue = queue_with(&["zzz1", "zzz2"]);
        let assembled = complete_map(&["zzz1", "zzz2"]);

        let report = run(dir.path(), &queue, &assembled, "release")
            .await
            .unwrap();

        assert_eq!(report.renames.len(), 2);
        assert!(report
            .renames
            .iter()
            .all(|r| r.reason == RenameReason::Guessed));
        assert!(dir.path().join("release.part01.rar").exists());
        assert!(dir.path().join("release.part02.rar").exists());
    }

    #[tokio::test]
    async fn a_single_rar_guess_gets_no_volume_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("onlyone"), b"Rar!\x1a\x07\x00rest").unwrap();

        let queue = queue_with(&["onlyone"]);
        let assembled = complete_map(&["onlyone"]);

        let report = run(dir.path(), &queue, &assembled, "release")
            .await
            .unwrap();

        assert_eq!(report.renames.len(), 1);
        assert_eq!(report.renames[0].new_name, "release.rar");
    }

    #[tokio::test]
    async fn files_that_do_not_sniff_as_anything_known_are_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("media.bin"), b"just some media bytes").unwrap();

        let queue = queue_with(&["media.bin"]);
        let assembled = complete_map(&["media.bin"]);

        let report = run(dir.path(), &queue, &assembled, "release")
            .await
            .unwrap();

        assert!(report.renames.is_empty());
        assert!(dir.path().join("media.bin").exists());
    }

    #[tokio::test]
    async fn existing_file_at_the_guessed_name_blocks_the_rename() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("onlyone"), b"Rar!\x1a\x07\x00rest").unwrap();
        std::fs::write(dir.path().join("release.rar"), b"already here").unwrap();

        let queue = queue_with(&["onlyone"]);
        let assembled = complete_map(&["onlyone"]);

        let report = run(dir.path(), &queue, &assembled, "release")
            .await
            .unwrap();

        assert!(report.renames.is_empty());
        assert!(dir.path().join("onlyone").exists());
        assert_eq!(
            std::fs::read(dir.path().join("release.rar")).unwrap(),
            b"already here"
        );
    }

    #[tokio::test]
    async fn incomplete_files_are_never_rename_candidates() {
        let dir = tempfile::tempdir().unwrap();
        // No file actually written for "missing.bin" — Incomplete means
        // assemble wrote nothing.
        let queue = queue_with(&["missing.bin"]);
        let mut assembled = HashMap::new();
        assembled.insert(
            "missing.bin".to_string(),
            AssembleOutcome::Incomplete {
                missing_parts: vec![1],
            },
        );

        let report = run(dir.path(), &queue, &assembled, "release")
            .await
            .unwrap();
        assert!(report.renames.is_empty());
    }
}
