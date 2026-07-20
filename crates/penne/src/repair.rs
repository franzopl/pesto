//! PAR2 verification and repair for assembled downloads.
//!
//! Wraps [`pesto::par2`] (the re-exported `parmesan` crate), which already
//! implements PAR2 verify and repair — see `crates/parmesan/ROADMAP.md`
//! Phase 22. `penne` does not reimplement PAR2; it only needs to point the
//! existing engine at the directory [`crate::assemble`] wrote into.
//!
//! This is also where assembly's shortfalls get a second chance: a file
//! [`crate::assemble::AssembleOutcome::Incomplete`] left unwritten (missing
//! segments) is exactly parmesan's `FileStatus::Missing` — `parmesan::repair`
//! recreates it whole from recovery blocks, no reassembly required. A
//! [`crate::assemble::AssembleOutcome::ChecksumMismatch`] file is
//! `FileStatus::Damaged` — patched in place at the bad slices only.
//!
//! PAR2 verify/repair is synchronous, CPU/IO-bound work (Reed-Solomon over
//! potentially large files), so it runs on a blocking task via
//! `tokio::task::spawn_blocking` rather than on the async executor — the
//! same pattern `pesto`'s own poster uses for PAR2 work (see
//! `crates/pesto/src/upload.rs`).
//!
//! Before paying for that full verify pass, [`quick_check_all`] tries
//! [`crate::quickcheck`]'s PAR2 quick-check: when [`crate::assemble`]
//! already computed every file's real CRC-32 while writing it (nothing was
//! `Incomplete`/`ChecksumMismatch`) and each one matches what the PAR2
//! recovery set's IFSC data implies, the whole release is provably intact
//! without a single extra byte read — see that module's doc comment for
//! why this only ever *skips* verification in the fully-intact case, never
//! decides which files are damaged.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pesto::par2::recovery_set::RecoverySet;
use pesto::par2::repair::{repair as par2_repair, RepairOptions, RepairPlan};
use pesto::par2::verify::{verify_with_progress as par2_verify_with_progress, VerifyReport};

use crate::assemble::AssembleOutcome;

/// One slice (or a whole missing file) the full PAR2 verify pass has just
/// accounted for, for a live progress bar — mirrors [`crate::check::CheckProgress`].
/// Never sent when [`quick_check_all`] skips the full pass entirely, since
/// then there's nothing to report progress on.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerifyProgress {
    pub file_name: String,
    pub slices_done: usize,
    pub total_slices: usize,
}

pub type VerifyProgressSender = tokio::sync::mpsc::UnboundedSender<VerifyProgress>;
pub type VerifyProgressReceiver = tokio::sync::mpsc::UnboundedReceiver<VerifyProgress>;

/// Create a fresh verify-progress channel.
pub fn channel() -> (VerifyProgressSender, VerifyProgressReceiver) {
    tokio::sync::mpsc::unbounded_channel()
}

/// Outcome of checking a downloaded release against its PAR2 recovery data.
#[derive(Debug)]
pub enum RepairOutcome {
    /// No `.par2` file was found directly under the directory — nothing to
    /// verify against. Not necessarily an error: not every release ships
    /// PAR2 recovery data.
    NoRecoveryData,
    /// Every file matched its checksum; nothing needed repair.
    Ok,
    /// Damage and/or missing files were found and fully repaired in place.
    Repaired(RepairPlan),
    /// Damage was found but there isn't enough recovery data to fix it.
    NotRepairable(VerifyReport),
}

/// Find a `.par2` file directly under `dir` that belongs to *this*
/// release — i.e. whose file name is in `known_files`.
///
/// Per the PAR2 spec, every file in a recovery set — the index and every
/// recovery volume alike — carries the same Main/File-Description/IFSC
/// packets; only the recovery blocks differ between volumes. So any single
/// `.par2` file *belonging to the set* is a valid starting point for
/// [`RecoverySet::load`], which itself scans `dir` for the rest of the set.
/// This does not validate that the file is actually well-formed PAR2 —
/// [`RecoverySet::load`] does that.
///
/// `known_files` matters because `dir` is not necessarily scoped to one
/// release: `penne download`'s destination directory defaults to a single
/// shared `download_dir`, so leftover `.par2` files from a *previous*,
/// unrelated download can still be sitting right next to this release's
/// own files. Picking the first `.par2` file `read_dir` happens to return
/// — filesystem order, not creation order — would then hand
/// [`RecoverySet::load`] a completely different release's recovery set:
/// it would "verify" fine (that other release is presumably already
/// intact) while this release's real damage goes unrepaired, silently.
/// Restricting candidates to `known_files` (the file names this download's
/// own queue actually produced) rules that out.
pub fn find_par2_index(dir: &Path, known_files: &HashSet<String>) -> Result<Option<PathBuf>> {
    let mut found = None;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let is_known = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| known_files.contains(n));
        if path.is_file()
            && is_known
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("par2"))
        {
            found = Some(path);
            break;
        }
    }
    Ok(found)
}

/// Verify every file described by the recovery set found under `dir`
/// against the copies [`crate::assemble`] wrote there, repairing in place
/// when damage or missing files are found and enough recovery data is
/// available.
///
/// `assembled` is the same map `download_queue`/`penne download` already
/// has (keyed by file name) — passed through so the PAR2 quick-check can
/// use each file's already-computed real CRC-32 instead of re-reading it.
/// An empty map (or one missing an entry) just means the quick-check can't
/// apply to that file; behavior falls back to the full verify pass exactly
/// as if this parameter didn't exist.
///
/// `known_files` is passed straight to [`find_par2_index`] — see its doc
/// comment for why `dir` alone isn't a safe enough scope. Deliberately a
/// separate parameter from `assembled` rather than derived from its keys:
/// the caller may have renamed files on disk since `assembled` was built
/// (e.g. [`crate::deobfuscate::run`] recovering real names for an
/// obfuscated release), in which case `assembled`'s keys are stale and
/// `known_files` must reflect the current on-disk names instead.
pub async fn verify_and_repair(
    dir: &Path,
    assembled: &HashMap<String, AssembleOutcome>,
    known_files: &HashSet<String>,
    progress: Option<VerifyProgressSender>,
) -> Result<RepairOutcome> {
    let dir = dir.to_path_buf();
    let assembled = assembled.clone();
    let known_files = known_files.clone();
    tokio::task::spawn_blocking(move || {
        verify_and_repair_blocking(&dir, &assembled, &known_files, progress)
    })
    .await
    .context("PAR2 verify/repair task panicked")?
}

fn verify_and_repair_blocking(
    dir: &Path,
    assembled: &HashMap<String, AssembleOutcome>,
    known_files: &HashSet<String>,
    progress: Option<VerifyProgressSender>,
) -> Result<RepairOutcome> {
    let Some(index) = find_par2_index(dir, known_files)? else {
        return Ok(RepairOutcome::NoRecoveryData);
    };

    let set = RecoverySet::load(&index)
        .with_context(|| format!("loading PAR2 recovery set from {}", index.display()))?;

    if quick_check_all(&set, assembled) {
        return Ok(RepairOutcome::Ok);
    }

    let report = par2_verify_with_progress(&set, dir, |p| {
        if let Some(tx) = &progress {
            let _ = tx.send(VerifyProgress {
                file_name: p.file_name.to_string(),
                slices_done: p.slices_done,
                total_slices: p.total_slices,
            });
        }
    })
    .with_context(|| format!("verifying files under {}", dir.display()))?;

    if report.is_ok() {
        return Ok(RepairOutcome::Ok);
    }
    if !report.is_repairable() {
        return Ok(RepairOutcome::NotRepairable(report));
    }

    let plan = par2_repair(&set, &report, dir, &RepairOptions::default())
        .with_context(|| format!("repairing files under {}", dir.display()))?;
    Ok(RepairOutcome::Repaired(plan))
}

/// `true` only when *every* file in `set` has a known real CRC-32 from
/// `assembled` (i.e. nothing was `Incomplete`/`ChecksumMismatch`) and
/// [`crate::quickcheck::looks_intact`] confirms each one against its own
/// IFSC data. Any file that's unknown to `assembled`, inconclusive (no
/// IFSC data for it), or mismatched falls back to the full `par2_verify`
/// pass for *every* file, not just that one — there is no partial-verify
/// entry point to hand a subset of files to, and the common case this
/// exists for (a fully intact release) is precisely the case where that
/// doesn't matter.
fn quick_check_all(set: &RecoverySet, assembled: &HashMap<String, AssembleOutcome>) -> bool {
    if set.files.is_empty() {
        return false;
    }
    set.files.iter().all(|file| {
        let known_crc32 = match assembled.get(&file.name) {
            Some(AssembleOutcome::Complete { actual_crc32 })
            | Some(AssembleOutcome::CompleteUnverified { actual_crc32 }) => *actual_crc32,
            _ => return false,
        };
        crate::quickcheck::looks_intact(file, set.slice_size, known_crc32) == Some(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_par2_file_regardless_of_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("movie.vol03+04.par2"), b"not real par2").unwrap();
        std::fs::write(dir.path().join("movie.mkv"), b"data").unwrap();

        let known: HashSet<String> = ["movie.vol03+04.par2", "movie.mkv"]
            .into_iter()
            .map(String::from)
            .collect();
        let found = find_par2_index(dir.path(), &known).unwrap();
        assert_eq!(found, Some(dir.path().join("movie.vol03+04.par2")));
    }

    #[test]
    fn no_par2_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("movie.mkv"), b"data").unwrap();

        let known: HashSet<String> = ["movie.mkv".to_string()].into_iter().collect();
        assert_eq!(find_par2_index(dir.path(), &known).unwrap(), None);
    }

    #[test]
    fn ignores_a_par2_file_left_over_from_a_different_release_in_a_shared_directory() {
        // `download_dir` is shared across every `penne download` run unless
        // `--out-dir` is given, so a previous, unrelated release's `.par2`
        // files can still be sitting in the same directory. Without
        // `known_files` scoping, `read_dir`'s (filesystem-order-dependent)
        // first `.par2` match could point `RecoverySet::load` at that
        // unrelated release entirely, silently skipping this one's own.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other-release.par2"), b"not real par2").unwrap();
        std::fs::write(dir.path().join("movie.mkv"), b"data").unwrap();

        let known: HashSet<String> = ["movie.mkv".to_string()].into_iter().collect();
        assert_eq!(find_par2_index(dir.path(), &known).unwrap(), None);
    }
}
