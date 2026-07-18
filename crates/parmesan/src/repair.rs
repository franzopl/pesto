//! High-level repair orchestration: turn a [`VerifyReport`] into a
//! [`RepairPlan`], reconstructing damaged or missing slices via
//! [`crate::decoder::RecoveryDecoder`] and writing them back to disk.
//!
//! # A caveat on cross-tool compatibility
//!
//! Reconstruction assumes the global order of input slices matches the
//! order [`RecoverySet::files`] presents them in — ascending File ID, per
//! spec (see that module's docs). `parmesan create` has fed slices to the
//! encoder in that order since the fix noted in `ROADMAP.md` Phase 22;
//! multi-file `.par2` sets created by older `parmesan` builds do not
//! satisfy this and **will silently reconstruct incorrect bytes** if
//! repaired here, because the wrong Reed-Solomon coefficients get applied
//! to the wrong slices. There is no way to detect this after the fact from
//! the PAR2 data alone — the per-slice checksum re-verification this module
//! performs before writing anything is the safety net: a mismatch there
//! aborts the repair for that file instead of writing corrupted data.
//! Single-file recovery sets are never affected by the ordering issue.

use crate::decoder::RecoveryDecoder;
use crate::encoder::slice_checksum;
use crate::recovery_set::{FileEntry, RecoverySet};
use crate::verify::{FileStatus, VerifyReport};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Options controlling how [`repair`] writes reconstructed data.
#[derive(Debug, Clone, Default)]
pub struct RepairOptions {
    /// Write repaired files under this directory instead of overwriting the
    /// originals in place. Damaged (partially bad) files are first copied
    /// here in full, then patched; missing files are written fresh.
    pub out_dir: Option<PathBuf>,
    /// Compute and checksum-verify reconstructed data without writing
    /// anything to disk.
    pub dry_run: bool,
}

/// One file's repair outcome.
#[derive(Debug, Clone)]
pub struct RepairedFile {
    pub name: String,
    /// Where the repaired file was (or, for a dry run, would be) written.
    pub path: PathBuf,
    pub slices_repaired: usize,
    /// Every reconstructed slice's checksum was confirmed against the
    /// recovery set's IFSC packet before this file was written (or, for a
    /// dry run, before this entry was produced) — always `true` today,
    /// since a mismatch aborts the whole repair instead of producing a
    /// `RepairedFile` with unverified data.
    pub verified: bool,
}

/// Result of a [`repair`] run.
#[derive(Debug, Clone)]
pub struct RepairPlan {
    pub repaired_files: Vec<RepairedFile>,
    pub dry_run: bool,
}

/// Reconstruct every damaged/missing slice `report` identified and write the
/// results back to disk (or, if `options.dry_run`, just reconstruct and
/// checksum-verify without writing).
///
/// # Errors
///
/// Returns an error if `report` isn't repairable (not enough recovery
/// blocks), if the Reed-Solomon solve fails, if a reconstructed slice's
/// checksum doesn't match the recovery set's IFSC packet, or on I/O
/// failure.
pub fn repair(
    set: &RecoverySet,
    report: &VerifyReport,
    base_dir: &Path,
    options: &RepairOptions,
) -> Result<RepairPlan> {
    if report.is_ok() {
        return Ok(RepairPlan {
            repaired_files: Vec::new(),
            dry_run: options.dry_run,
        });
    }
    anyhow::ensure!(
        report.is_repairable(),
        "not enough recovery data to repair: {} bad slice(s), only {} recovery block(s) available",
        report.total_bad_slices(),
        report.available_recovery_blocks
    );
    anyhow::ensure!(
        set.files.len() == report.files.len(),
        "recovery set and verify report disagree on file count"
    );

    let slice_size = set.slice_size as usize;

    // Global-slice-index bookkeeping: which file/local-index each global
    // index belongs to, in the same ascending-File-ID order used to encode.
    let mut file_of = Vec::new();
    let mut local_of = Vec::new();
    let mut file_start = Vec::with_capacity(set.files.len());
    for (fi, f) in set.files.iter().enumerate() {
        file_start.push(file_of.len());
        for li in 0..f.slice_checksums.len() {
            file_of.push(fi);
            local_of.push(li);
        }
    }
    let total_input_slices = file_of.len();

    let mut missing = Vec::new();
    for (fi, fr) in report.files.iter().enumerate() {
        for &li in &fr.bad_slice_indices {
            missing.push(file_start[fi] + li);
        }
    }
    missing.sort_unstable();

    let reconstructed = {
        let mut reader = SliceReader {
            set,
            base_dir,
            file_of: &file_of,
            local_of: &local_of,
            open: None,
        };
        let dec = RecoveryDecoder::new(slice_size, total_input_slices, missing);
        dec.reconstruct(|j| reader.read(j), &set.recovery_blocks)
            .context("reconstructing missing slices")?
    };

    // Group reconstructed slices back by file.
    let mut by_file: BTreeMap<usize, Vec<(usize, Vec<u8>)>> = BTreeMap::new();
    for (global, data) in reconstructed {
        by_file
            .entry(file_of[global])
            .or_default()
            .push((local_of[global], data));
    }

    let mut repaired_files = Vec::new();
    for (fi, mut slices) in by_file {
        slices.sort_by_key(|(li, _)| *li);
        let entry = &set.files[fi];
        let status = report.files[fi].status;

        // Confirm every reconstructed slice against its expected checksum
        // *before* writing anything — see the module-level compatibility
        // caveat for why this matters.
        for (li, data) in &slices {
            let got = slice_checksum(data);
            let expected = &entry.slice_checksums[*li];
            anyhow::ensure!(
                got.md5 == expected.md5 && got.crc32 == expected.crc32,
                "reconstructed slice {li} of `{}` does not match its expected checksum; \
                 no data was written for this file",
                entry.name
            );
        }

        let dest_dir = options.out_dir.as_deref().unwrap_or(base_dir);
        let dest_path = dest_dir.join(&entry.name);

        if !options.dry_run {
            write_repaired_file(entry, status, base_dir, &dest_path, slice_size, &slices)?;
        }

        repaired_files.push(RepairedFile {
            name: entry.name.clone(),
            path: dest_path,
            slices_repaired: slices.len(),
            verified: true,
        });
    }

    Ok(RepairPlan {
        repaired_files,
        dry_run: options.dry_run,
    })
}

/// Reads known (non-missing) slices from disk for [`RecoveryDecoder`],
/// keeping the last file opened since global indices are visited in
/// ascending order and files occupy contiguous index ranges.
struct SliceReader<'a> {
    set: &'a RecoverySet,
    base_dir: &'a Path,
    file_of: &'a [usize],
    local_of: &'a [usize],
    open: Option<(usize, std::fs::File)>,
}

impl SliceReader<'_> {
    fn read(&mut self, global_index: usize) -> Result<Vec<u8>> {
        let file_index = self.file_of[global_index];
        let local_index = self.local_of[global_index];
        let slice_size = self.set.slice_size as usize;

        if self.open.as_ref().map(|(fi, _)| *fi) != Some(file_index) {
            let path = self.base_dir.join(&self.set.files[file_index].name);
            let file = std::fs::File::open(&path)
                .with_context(|| format!("opening `{}` to read a known slice", path.display()))?;
            self.open = Some((file_index, file));
        }
        let (_, file) = self.open.as_mut().unwrap();
        file.seek(SeekFrom::Start((local_index * slice_size) as u64))?;

        let mut buf = vec![0u8; slice_size];
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
        Ok(buf)
    }
}

/// Write `entry`'s reconstructed slices to `dest_path`.
///
/// `Missing` files are written from scratch; `Damaged` files are copied
/// whole from `base_dir` first (skipped when `dest_path` already *is* the
/// original — an in-place repair) so untouched slices survive, then patched
/// at each reconstructed slice's offset.
fn write_repaired_file(
    entry: &FileEntry,
    status: FileStatus,
    base_dir: &Path,
    dest_path: &Path,
    slice_size: usize,
    slices: &[(usize, Vec<u8>)],
) -> Result<()> {
    let total_slices = entry.slice_checksums.len();

    match status {
        FileStatus::Missing => {
            let mut out = std::fs::File::create(dest_path)
                .with_context(|| format!("creating `{}`", dest_path.display()))?;
            for (li, data) in slices {
                let write_len = slice_write_len(*li, total_slices, entry.length, slice_size);
                out.write_all(&data[..write_len])?;
            }
        }
        FileStatus::Damaged => {
            let original = base_dir.join(&entry.name);
            if dest_path != original {
                std::fs::copy(&original, dest_path).with_context(|| {
                    format!(
                        "copying `{}` to `{}` before repair",
                        original.display(),
                        dest_path.display()
                    )
                })?;
            }
            let mut out = std::fs::OpenOptions::new()
                .write(true)
                .open(dest_path)
                .with_context(|| format!("opening `{}` for repair", dest_path.display()))?;
            for (li, data) in slices {
                let write_len = slice_write_len(*li, total_slices, entry.length, slice_size);
                out.seek(SeekFrom::Start((*li * slice_size) as u64))?;
                out.write_all(&data[..write_len])?;
            }
        }
        FileStatus::Ok => {
            unreachable!("write_repaired_file is only called for files with bad slices")
        }
    }
    Ok(())
}

/// Number of real (unpadded) bytes to write for slice `local_index`: the
/// full slice size, except the file's last slice, which is truncated to
/// wherever the file actually ends.
fn slice_write_len(
    local_index: usize,
    total_slices: usize,
    file_length: u64,
    slice_size: usize,
) -> usize {
    if local_index + 1 == total_slices {
        (file_length - local_index as u64 * slice_size as u64) as usize
    } else {
        slice_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_fixture_set, FixtureFile};

    #[test]
    fn repairs_a_single_corrupted_slice_in_place() {
        let (dir, index) = build_fixture_set(
            "repair-corrupt",
            &[FixtureFile {
                name: "a.bin",
                data: (0..500u32).map(|i| i as u8).collect(),
            }],
            128,
            4,
        );

        let path = dir.join("a.bin");
        let mut bytes = std::fs::read(&path).unwrap();
        let original = bytes.clone();
        bytes[10] ^= 0xFF; // corrupt one byte inside slice 0
        std::fs::write(&path, &bytes).unwrap();

        let set = RecoverySet::load(&index).unwrap();
        let report = crate::verify::verify(&set, &dir).unwrap();
        assert!(report.is_repairable());

        let plan = repair(&set, &report, &dir, &RepairOptions::default()).unwrap();
        assert_eq!(plan.repaired_files.len(), 1);
        assert_eq!(plan.repaired_files[0].slices_repaired, 1);

        let repaired = std::fs::read(&path).unwrap();
        assert_eq!(repaired, original);

        let report_after = crate::verify::verify(&set, &dir).unwrap();
        assert!(report_after.is_ok());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn recreates_an_entirely_missing_file() {
        let (dir, index) = build_fixture_set(
            "repair-missing",
            &[FixtureFile {
                name: "a.bin",
                data: (0..777u32).map(|i| (i * 3) as u8).collect(),
            }],
            128,
            8,
        );

        let path = dir.join("a.bin");
        let original = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let set = RecoverySet::load(&index).unwrap();
        let report = crate::verify::verify(&set, &dir).unwrap();
        assert!(report.is_repairable());

        let plan = repair(&set, &report, &dir, &RepairOptions::default()).unwrap();
        assert_eq!(plan.repaired_files.len(), 1);

        let recreated = std::fs::read(&path).unwrap();
        assert_eq!(recreated, original);
        assert_eq!(recreated.len(), 777);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn repairs_multiple_damaged_files_into_a_separate_out_dir() {
        let (dir, index) = build_fixture_set(
            "repair-outdir",
            &[
                FixtureFile {
                    name: "a.bin",
                    data: (0..400u32).map(|i| i as u8).collect(),
                },
                FixtureFile {
                    name: "b.bin",
                    data: (0..600u32).map(|i| (i * 7) as u8).collect(),
                },
            ],
            128,
            8,
        );

        let a_path = dir.join("a.bin");
        let b_path = dir.join("b.bin");
        let a_original = std::fs::read(&a_path).unwrap();
        let b_original = std::fs::read(&b_path).unwrap();

        let mut a_bytes = a_original.clone();
        a_bytes[5] ^= 0xFF;
        std::fs::write(&a_path, &a_bytes).unwrap();
        let mut b_bytes = b_original.clone();
        b_bytes[300] ^= 0xFF;
        std::fs::write(&b_path, &b_bytes).unwrap();

        let set = RecoverySet::load(&index).unwrap();
        let report = crate::verify::verify(&set, &dir).unwrap();
        assert!(report.is_repairable());

        let out_dir = dir.join("restored");
        std::fs::create_dir_all(&out_dir).unwrap();
        let options = RepairOptions {
            out_dir: Some(out_dir.clone()),
            dry_run: false,
        };
        let plan = repair(&set, &report, &dir, &options).unwrap();
        assert_eq!(plan.repaired_files.len(), 2);

        assert_eq!(std::fs::read(out_dir.join("a.bin")).unwrap(), a_original);
        assert_eq!(std::fs::read(out_dir.join("b.bin")).unwrap(), b_original);
        // Originals in `dir` are untouched — still corrupted.
        assert_eq!(std::fs::read(&a_path).unwrap(), a_bytes);
        assert_eq!(std::fs::read(&b_path).unwrap(), b_bytes);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dry_run_verifies_without_writing() {
        let (dir, index) = build_fixture_set(
            "repair-dry-run",
            &[FixtureFile {
                name: "a.bin",
                data: (0..500u32).map(|i| i as u8).collect(),
            }],
            128,
            4,
        );

        let path = dir.join("a.bin");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[10] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();
        let corrupted = bytes;

        let set = RecoverySet::load(&index).unwrap();
        let report = crate::verify::verify(&set, &dir).unwrap();

        let options = RepairOptions {
            out_dir: None,
            dry_run: true,
        };
        let plan = repair(&set, &report, &dir, &options).unwrap();
        assert!(plan.dry_run);
        assert_eq!(plan.repaired_files.len(), 1);
        assert!(plan.repaired_files[0].verified);

        // Nothing was written to disk.
        assert_eq!(std::fs::read(&path).unwrap(), corrupted);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn already_ok_report_is_a_no_op() {
        let (dir, index) = build_fixture_set(
            "repair-noop",
            &[FixtureFile {
                name: "a.bin",
                data: vec![1u8; 300],
            }],
            128,
            4,
        );

        let set = RecoverySet::load(&index).unwrap();
        let report = crate::verify::verify(&set, &dir).unwrap();
        assert!(report.is_ok());

        let plan = repair(&set, &report, &dir, &RepairOptions::default()).unwrap();
        assert!(plan.repaired_files.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn not_repairable_report_is_an_error() {
        let (dir, index) = build_fixture_set(
            "repair-not-repairable",
            &[FixtureFile {
                name: "a.bin",
                data: vec![9u8; 500],
            }],
            128,
            1,
        );

        std::fs::write(dir.join("a.bin"), vec![0u8; 500]).unwrap(); // wipe every slice

        let set = RecoverySet::load(&index).unwrap();
        let report = crate::verify::verify(&set, &dir).unwrap();
        assert!(!report.is_repairable());

        let result = repair(&set, &report, &dir, &RepairOptions::default());
        assert!(result.is_err());

        std::fs::remove_dir_all(&dir).ok();
    }
}
