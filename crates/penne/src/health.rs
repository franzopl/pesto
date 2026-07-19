//! Repairability estimate: compares how much data is missing/damaged after
//! a fetch against how much PAR2 recovery data is actually available, so a
//! release that already looks unrepairable can be flagged before paying for
//! a full [`pesto::par2::verify::verify`] pass (a whole-file re-hash —
//! Phase 13 of `ROADMAP.md` measured this at several seconds even in
//! `--release` for a large file, longer in a debug build).
//!
//! Originally scoped (see `ROADMAP.md` Phase 15) as a check *before* any
//! download starts, mirroring `nzbget`'s `QueueCoordinator::CheckHealth`.
//! Not feasible in that form: [`pesto::par2::recovery_set::RecoverySet::load`]
//! needs the `.par2` index and its recovery volumes already sitting on
//! disk to know how much recovery data exists at all, and nothing is on
//! disk before a download begins. `nzbget`'s own version has the identical
//! constraint — `CheckHealth` is called per-completed-article as a download
//! *progresses*, never as a pre-flight step, for exactly this reason. This
//! runs the same comparison once the fetch phase has completed (so PAR2
//! data, if the release shipped any, already exists on disk), as a cheap
//! gate in front of the expensive verify pass rather than a replacement
//! for it — see [`HealthCheck::looks_repairable`]'s doc comment for why
//! this is only ever used to *warn*, never to skip `verify_and_repair`
//! outright.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use pesto::par2::recovery_set::RecoverySet;

use crate::assemble::AssembleOutcome;
use crate::queue::DownloadQueue;
use crate::repair::find_par2_index;

/// How much repair work is needed vs. how much recovery data is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthCheck {
    pub damaged_bytes: u64,
    /// Upper bound on what the available recovery blocks could reconstruct
    /// (`recovery_blocks.len() * slice_size`) — an upper bound, not a
    /// guarantee: PAR2 repairs at slice granularity, and real damage
    /// doesn't always align to slice boundaries as cleanly as a raw byte
    /// count implies.
    pub available_recovery_bytes: u64,
}

impl HealthCheck {
    /// `false` only when recovery data is already clearly insufficient.
    /// [`damaged_bytes`] sums the *wire* (yEnc-encoded) size of the
    /// affected segments as a proxy for their decoded size — the same
    /// slight overestimate [`crate::diskspace::required_bytes`] makes, and
    /// for the same reason (encoding always expands data a little). That
    /// means this can occasionally say "looks unrepairable" a little too
    /// eagerly, never the reverse — which is exactly why the caller must
    /// only use this to *warn* the user early, and must always still run
    /// the real, byte-exact [`pesto::par2::verify::verify`] afterward
    /// rather than skip it on a `false` here.
    pub fn looks_repairable(&self) -> bool {
        self.damaged_bytes <= self.available_recovery_bytes
    }
}

/// Sum of the wire byte size of every segment that still needs repairing
/// after assembly: every missing part of an [`AssembleOutcome::Incomplete`]
/// file, and every `bad_parts` entry of an
/// [`AssembleOutcome::ChecksumMismatch`] file. `Complete`/
/// `CompleteUnverified` files contribute nothing.
pub fn damaged_bytes(queue: &DownloadQueue, assembled: &HashMap<String, AssembleOutcome>) -> u64 {
    let mut total = 0u64;
    for file in &queue.files {
        let Some(outcome) = assembled.get(&file.name) else {
            continue;
        };
        let bad_parts: &[u32] = match outcome {
            AssembleOutcome::Incomplete { missing_parts } => missing_parts,
            AssembleOutcome::ChecksumMismatch { bad_parts, .. } => bad_parts,
            AssembleOutcome::Complete | AssembleOutcome::CompleteUnverified => continue,
        };
        total += file
            .segments
            .iter()
            .filter(|seg| bad_parts.contains(&seg.part))
            .map(|seg| seg.bytes)
            .sum::<u64>();
    }
    total
}

/// Look for a PAR2 recovery set under `dest_dir` (the same discovery
/// [`crate::repair`] uses) and, if one is found, compare `damaged` bytes
/// against how much its recovery blocks could plausibly reconstruct.
///
/// `Ok(None)` when no PAR2 index is present at all — there is nothing to
/// compare against, and [`crate::repair::verify_and_repair`] already
/// reports that case on its own (`RepairOutcome::NoRecoveryData`).
pub fn evaluate(dest_dir: &Path, damaged_bytes: u64) -> Result<Option<HealthCheck>> {
    let Some(index_path) = find_par2_index(dest_dir)? else {
        return Ok(None);
    };
    let recovery_set = RecoverySet::load(&index_path)
        .with_context(|| format!("loading PAR2 recovery set from {}", index_path.display()))?;
    let available_recovery_bytes =
        recovery_set.recovery_blocks.len() as u64 * recovery_set.slice_size;
    Ok(Some(HealthCheck {
        damaged_bytes,
        available_recovery_bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{QueuedFile, QueuedSegment};

    fn file_with_segments(name: &str, sizes: &[u64]) -> QueuedFile {
        QueuedFile {
            name: name.to_string(),
            segments: sizes
                .iter()
                .enumerate()
                .map(|(i, &bytes)| QueuedSegment {
                    message_id: format!("id{i}@test"),
                    part: i as u32 + 1,
                    bytes,
                })
                .collect(),
        }
    }

    #[test]
    fn incomplete_file_counts_only_its_missing_parts() {
        let queue = DownloadQueue {
            files: vec![file_with_segments("a.bin", &[100, 200, 300])],
        };
        let mut assembled = HashMap::new();
        assembled.insert(
            "a.bin".to_string(),
            AssembleOutcome::Incomplete {
                missing_parts: vec![2],
            },
        );
        assert_eq!(damaged_bytes(&queue, &assembled), 200);
    }

    #[test]
    fn checksum_mismatch_counts_only_bad_parts() {
        let queue = DownloadQueue {
            files: vec![file_with_segments("a.bin", &[100, 200, 300])],
        };
        let mut assembled = HashMap::new();
        assembled.insert(
            "a.bin".to_string(),
            AssembleOutcome::ChecksumMismatch {
                expected: None,
                actual: 0,
                bad_parts: vec![1, 3],
            },
        );
        assert_eq!(damaged_bytes(&queue, &assembled), 400);
    }

    #[test]
    fn complete_files_contribute_nothing() {
        let queue = DownloadQueue {
            files: vec![file_with_segments("a.bin", &[100, 200])],
        };
        let mut assembled = HashMap::new();
        assembled.insert("a.bin".to_string(), AssembleOutcome::Complete);
        assert_eq!(damaged_bytes(&queue, &assembled), 0);
    }

    #[test]
    fn sums_across_multiple_damaged_files() {
        let queue = DownloadQueue {
            files: vec![
                file_with_segments("a.bin", &[100, 200]),
                file_with_segments("b.bin", &[50, 50, 50]),
            ],
        };
        let mut assembled = HashMap::new();
        assembled.insert(
            "a.bin".to_string(),
            AssembleOutcome::Incomplete {
                missing_parts: vec![1, 2],
            },
        );
        assembled.insert(
            "b.bin".to_string(),
            AssembleOutcome::ChecksumMismatch {
                expected: None,
                actual: 0,
                bad_parts: vec![2],
            },
        );
        assert_eq!(damaged_bytes(&queue, &assembled), 350);
    }

    #[test]
    fn no_par2_index_evaluates_to_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), b"data").unwrap();
        assert_eq!(evaluate(dir.path(), 1_000).unwrap(), None);
    }

    #[test]
    fn health_check_reports_repairable_when_damage_fits_available_recovery() {
        let check = HealthCheck {
            damaged_bytes: 100,
            available_recovery_bytes: 100,
        };
        assert!(check.looks_repairable());
    }

    #[test]
    fn health_check_reports_unrepairable_when_damage_exceeds_available_recovery() {
        let check = HealthCheck {
            damaged_bytes: 101,
            available_recovery_bytes: 100,
        };
        assert!(!check.looks_repairable());
    }
}
