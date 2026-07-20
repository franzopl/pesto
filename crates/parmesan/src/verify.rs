//! Verification of input files against an existing PAR2 recovery set.
//!
//! Unlike repair (Phase 22d), verification never touches Reed-Solomon
//! coefficients — it only re-hashes files on disk and compares against the
//! File Description / IFSC packets already parsed into a [`RecoverySet`].

use crate::encoder::slice_checksum;
use crate::recovery_set::RecoverySet;
use anyhow::{Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Verification outcome for one file in the recovery set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Every slice's checksum matched.
    Ok,
    /// The file exists but one or more slices don't match.
    Damaged,
    /// The file does not exist at the expected path at all.
    Missing,
}

/// Per-file verification result.
#[derive(Debug, Clone)]
pub struct FileReport {
    pub name: String,
    pub status: FileStatus,
    /// Total slices this file is expected to have, per the IFSC packet.
    pub total_slices: usize,
    /// Slices that need reconstruction (0 when `status` is `Ok`; equal to
    /// `total_slices` when `status` is `Missing`).
    pub bad_slices: usize,
    /// Indices (0-based, within this file) of the slices that need
    /// reconstruction, ascending. Empty when `status` is `Ok`; every index
    /// `0..total_slices` when `status` is `Missing`. [`crate::repair`] uses
    /// this directly to know which slices to hand to [`crate::decoder`].
    pub bad_slice_indices: Vec<usize>,
}

/// Full verification result for a recovery set.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub files: Vec<FileReport>,
    /// Recovery blocks available on disk for this recovery set.
    pub available_recovery_blocks: usize,
}

impl VerifyReport {
    /// True if every file matched every checksum — nothing to repair.
    pub fn is_ok(&self) -> bool {
        self.files.iter().all(|f| f.status == FileStatus::Ok)
    }

    /// Total slices across the whole recovery set that need reconstruction.
    pub fn total_bad_slices(&self) -> usize {
        self.files.iter().map(|f| f.bad_slices).sum()
    }

    /// True if there's damage but enough recovery blocks exist to fix it.
    ///
    /// Reed-Solomon over GF(2^16) is a maximum-distance-separable code: any
    /// `m` missing input blocks are reconstructible from *any* `m` available
    /// recovery blocks, so "enough recovery blocks" is exactly this count
    /// comparison — no need to know which blocks are missing yet.
    pub fn is_repairable(&self) -> bool {
        !self.is_ok() && self.total_bad_slices() <= self.available_recovery_blocks
    }

    /// Exit code matching the PAR2 convention: 0 = OK, 1 = repairable,
    /// 2 = damaged beyond what the available recovery data can fix.
    pub fn exit_code(&self) -> i32 {
        if self.is_ok() {
            0
        } else if self.is_repairable() {
            1
        } else {
            2
        }
    }
}

/// Verify every file in `set` against copies found under `base_dir`.
pub fn verify(set: &RecoverySet, base_dir: &Path) -> Result<VerifyReport> {
    verify_with_progress(set, base_dir, |_| {})
}

/// One slice (or an entire missing file, accounted for in one step) that
/// [`verify_with_progress`] has just finished checking. `slices_done` and
/// `total_slices` count across the *whole* recovery set, not just the
/// current file, so callers can drive a single overall progress bar
/// straight off this value without tracking per-file totals themselves.
#[derive(Debug, Clone, Copy)]
pub struct VerifyProgress<'a> {
    pub file_name: &'a str,
    pub slices_done: usize,
    pub total_slices: usize,
}

/// Same as [`verify`], but calls `on_progress` after every slice is read
/// and checksummed (a missing file's slices are accounted for in one step,
/// since there's nothing to read) — the only way to observe liveness
/// during what can otherwise be a long, silent re-hash of every file in a
/// large release.
pub fn verify_with_progress(
    set: &RecoverySet,
    base_dir: &Path,
    mut on_progress: impl FnMut(VerifyProgress),
) -> Result<VerifyReport> {
    let slice_size = set.slice_size as usize;
    let total_slices: usize = set.files.iter().map(|f| f.slice_checksums.len()).sum();
    let mut slices_done = 0usize;
    let mut files = Vec::with_capacity(set.files.len());

    for entry in &set.files {
        let path = base_dir.join(&entry.name);
        let total_file_slices = entry.slice_checksums.len();

        if !path.is_file() {
            slices_done += total_file_slices;
            on_progress(VerifyProgress {
                file_name: &entry.name,
                slices_done,
                total_slices,
            });
            files.push(FileReport {
                name: entry.name.clone(),
                status: FileStatus::Missing,
                total_slices: total_file_slices,
                bad_slices: total_file_slices,
                bad_slice_indices: (0..total_file_slices).collect(),
            });
            continue;
        }

        let mut file = File::open(&path)
            .with_context(|| format!("opening `{}` for verification", path.display()))?;

        let mut bad_slice_indices = Vec::new();
        let mut buf = vec![0u8; slice_size];
        for (i, expected) in entry.slice_checksums.iter().enumerate() {
            let read = read_slice_padded(&mut file, &mut buf)?;
            let got = slice_checksum(&buf);
            if read == 0 || got.md5 != expected.md5 || got.crc32 != expected.crc32 {
                bad_slice_indices.push(i);
            }
            slices_done += 1;
            on_progress(VerifyProgress {
                file_name: &entry.name,
                slices_done,
                total_slices,
            });
        }

        files.push(FileReport {
            name: entry.name.clone(),
            status: if bad_slice_indices.is_empty() {
                FileStatus::Ok
            } else {
                FileStatus::Damaged
            },
            total_slices: total_file_slices,
            bad_slices: bad_slice_indices.len(),
            bad_slice_indices,
        });
    }

    Ok(VerifyReport {
        files,
        available_recovery_blocks: set.recovery_blocks.len(),
    })
}

/// Read up to `buf.len()` bytes from `file`, zero-padding any shortfall
/// (end of file reached early) so the checksum matches the zero-padded
/// slice convention used at encode time. Returns the number of real bytes
/// read (0 means the file has no more data at this position at all).
fn read_slice_padded(file: &mut File, buf: &mut [u8]) -> Result<usize> {
    let slice_size = buf.len();
    let mut total = 0usize;
    while total < slice_size {
        match file.read(&mut buf[total..])? {
            0 => break,
            n => total += n,
        }
    }
    if total < slice_size {
        buf[total..].fill(0);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery_set::RecoverySet;
    use crate::test_support::{build_fixture_set, FixtureFile};

    #[test]
    fn intact_files_report_ok() {
        let (dir, index) = build_fixture_set(
            "verify-ok",
            &[
                FixtureFile {
                    name: "a.bin",
                    data: vec![1u8; 300],
                },
                FixtureFile {
                    name: "b.bin",
                    data: vec![2u8; 500],
                },
            ],
            128,
            4,
        );

        let set = RecoverySet::load(&index).unwrap();
        let report = verify(&set, &dir).unwrap();

        assert!(report.is_ok());
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.total_bad_slices(), 0);
        assert!(report.files.iter().all(|f| f.status == FileStatus::Ok));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupted_byte_flags_exactly_one_bad_slice_and_stays_repairable() {
        let (dir, index) = build_fixture_set(
            "verify-corrupt",
            &[FixtureFile {
                name: "a.bin",
                data: vec![9u8; 500],
            }],
            128,
            4,
        );

        let path = dir.join("a.bin");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[10] ^= 0xFF; // corrupt one byte inside slice 0
        std::fs::write(&path, &bytes).unwrap();

        let set = RecoverySet::load(&index).unwrap();
        let report = verify(&set, &dir).unwrap();

        assert!(!report.is_ok());
        assert_eq!(report.total_bad_slices(), 1);
        assert!(report.is_repairable());
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.files[0].status, FileStatus::Damaged);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_counts_every_slice_as_bad() {
        let (dir, index) = build_fixture_set(
            "verify-missing",
            &[FixtureFile {
                name: "a.bin",
                data: vec![9u8; 500],
            }],
            128,
            4,
        );

        std::fs::remove_file(dir.join("a.bin")).unwrap();

        let set = RecoverySet::load(&index).unwrap();
        let report = verify(&set, &dir).unwrap();

        assert_eq!(report.files[0].status, FileStatus::Missing);
        assert_eq!(report.files[0].bad_slices, report.files[0].total_slices);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn more_damage_than_recovery_blocks_is_not_repairable() {
        // slice_size=128, file=500 bytes -> 4 slices, but only 1 recovery block.
        let (dir, index) = build_fixture_set(
            "verify-unrepairable",
            &[FixtureFile {
                name: "a.bin",
                data: vec![9u8; 500],
            }],
            128,
            1,
        );

        let path = dir.join("a.bin");
        std::fs::write(&path, vec![0u8; 500]).unwrap(); // wipe every slice

        let set = RecoverySet::load(&index).unwrap();
        let report = verify(&set, &dir).unwrap();

        assert!(!report.is_repairable());
        assert_eq!(report.exit_code(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }
}
