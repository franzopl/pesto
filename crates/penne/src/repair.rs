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

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pesto::par2::recovery_set::RecoverySet;
use pesto::par2::repair::{repair as par2_repair, RepairOptions, RepairPlan};
use pesto::par2::verify::{verify as par2_verify, VerifyReport};

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

/// Find a `.par2` file directly under `dir`.
///
/// Per the PAR2 spec, every file in a recovery set — the index and every
/// recovery volume alike — carries the same Main/File-Description/IFSC
/// packets; only the recovery blocks differ between volumes. So any single
/// `.par2` file is a valid starting point for [`RecoverySet::load`], which
/// itself scans `dir` for the rest of the set. This does not validate that
/// the file is actually well-formed PAR2 — [`RecoverySet::load`] does that.
pub fn find_par2_index(dir: &Path) -> Result<Option<PathBuf>> {
    let mut found = None;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_file()
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
pub async fn verify_and_repair(dir: &Path) -> Result<RepairOutcome> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || verify_and_repair_blocking(&dir))
        .await
        .context("PAR2 verify/repair task panicked")?
}

fn verify_and_repair_blocking(dir: &Path) -> Result<RepairOutcome> {
    let Some(index) = find_par2_index(dir)? else {
        return Ok(RepairOutcome::NoRecoveryData);
    };

    let set = RecoverySet::load(&index)
        .with_context(|| format!("loading PAR2 recovery set from {}", index.display()))?;
    let report = par2_verify(&set, dir)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_par2_file_regardless_of_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("movie.vol03+04.par2"), b"not real par2").unwrap();
        std::fs::write(dir.path().join("movie.mkv"), b"data").unwrap();

        let found = find_par2_index(dir.path()).unwrap();
        assert_eq!(found, Some(dir.path().join("movie.vol03+04.par2")));
    }

    #[test]
    fn no_par2_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("movie.mkv"), b"data").unwrap();

        assert_eq!(find_par2_index(dir.path()).unwrap(), None);
    }
}
