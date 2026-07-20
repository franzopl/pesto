//! Per-job port of `penne`'s own CLI pipeline
//! (`crates/penne/src/bin/penne.rs`'s `download()`): nzb load -> queue build
//! -> disk-space check -> `download_queue` -> deobfuscate -> (mode-gated)
//! PAR2 verify/repair -> extract -> cleanup -> cache clear.
//!
//! The only structural difference from the CLI: every `penne::ui::*::
//! spawn_renderer(rx)` call (which draws a terminal panel) is replaced here
//! by a forwarder task that updates this job's live state in [`AppState`]
//! and broadcasts a [`JobEventPayload::Progress`] over SSE instead. Both
//! forwarders (download, PAR2 verify) flush at most every [`FLUSH_INTERVAL`]
//! — the per-segment/per-slice event stream underneath is far chattier than
//! any UI needs to redraw at.
//!
//! Archive extraction still doesn't get its own live percentage: `penne::
//! extract` shells out to `7z`/`unrar` without parsing their stdout for
//! progress, so there is nothing to forward — only the status transition
//! (`Extracting`) is reported. PAR2 verify's percentage is scoped to
//! whichever file is *currently* being checked
//! ([`penne::repair::VerifyProgress`] doesn't carry a release-wide total)
//! — real, live data, just not a single release-wide bar.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::job::{JobFileProgress, JobStatus, JobVerifyProgress};
use crate::state::{AppState, JobEventPayload};

/// Minimum wall-clock gap between two progress flushes (`JobStore` write +
/// SSE broadcast) — the underlying event streams fire far more often than
/// any UI needs to redraw at. File-completion events flush immediately
/// regardless (infrequent, and worth showing right away).
const FLUSH_INTERVAL: Duration = Duration::from_millis(350);

pub async fn run_job(state: &Arc<AppState>, job_id: Uuid) -> Result<()> {
    let (nzb_path, dest_dir) = {
        let store = state.jobs.read().await;
        let job = store
            .active
            .as_ref()
            .filter(|j| j.id == job_id)
            .context("job is no longer active")?;
        (job.nzb_path.clone(), job.dest_dir.clone())
    };

    set_status(state, job_id, JobStatus::Downloading).await;

    let parsed = penne::nzb::load(&nzb_path)?;
    let queue = penne::queue::build(&parsed);

    let config = {
        let web_config = state.config.read().await;
        web_config.core.clone().select(&[])?.resolve()?
    };
    anyhow::ensure!(!config.server_tiers.is_empty(), "no [[servers]] configured");

    let required = penne::diskspace::required_bytes(&queue);
    let space = penne::diskspace::check(&dest_dir, required)?;
    anyhow::ensure!(
        space.is_enough(),
        "not enough free disk space in {}: need {}, only {} available",
        dest_dir.display(),
        pesto::progress::format_size(space.required),
        pesto::progress::format_size(space.available)
    );

    let (tx, mut rx) = penne::progress::channel();
    let forward_state = state.clone();
    let forward_task = tokio::spawn(async move {
        let mut files: HashMap<String, JobFileProgress> = HashMap::new();
        let mut bytes_done: u64 = 0;
        let mut speed_bps: f64 = 0.0;
        let mut last_flush = Instant::now();
        let mut last_flush_bytes: u64 = 0;

        while let Some(event) = rx.recv().await {
            let mut force_flush = false;
            match event {
                penne::progress::ProgressEvent::Started { files: entries } => {
                    files = entries
                        .into_iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                JobFileProgress {
                                    name: f.name,
                                    bytes_done: 0,
                                    bytes_total: f.bytes,
                                    done: false,
                                },
                            )
                        })
                        .collect();
                }
                penne::progress::ProgressEvent::SegmentDownloaded {
                    file_name, bytes, ..
                } => {
                    bytes_done += bytes;
                    if let Some(f) = files.get_mut(&file_name) {
                        f.bytes_done += bytes;
                    }
                }
                penne::progress::ProgressEvent::FileAssembled { file_name } => {
                    if let Some(f) = files.get_mut(&file_name) {
                        f.bytes_done = f.bytes_total;
                        f.done = true;
                    }
                    force_flush = true;
                }
                penne::progress::ProgressEvent::SegmentMissing { .. }
                | penne::progress::ProgressEvent::SegmentCorrupt { .. } => {
                    // Not reflected in live progress; the final
                    // `DownloadOutcome` (checked below) still reports these.
                }
            }

            if force_flush || last_flush.elapsed() >= FLUSH_INTERVAL {
                flush_download_progress(
                    &forward_state,
                    job_id,
                    bytes_done,
                    &files,
                    &mut speed_bps,
                    &mut last_flush,
                    &mut last_flush_bytes,
                )
                .await;
            }
        }
        // Unconditional final flush so the state right before the channel
        // closed (which a mid-interval throttle may have withheld) is
        // reflected before the next phase's status transition overwrites it.
        flush_download_progress(
            &forward_state,
            job_id,
            bytes_done,
            &files,
            &mut speed_bps,
            &mut last_flush,
            &mut last_flush_bytes,
        )
        .await;
    });

    let outcome = penne::download::download_queue(
        &queue,
        &config.server_tiers,
        &dest_dir,
        config.retries,
        Some(tx),
    )
    .await?;
    // `download_queue` owns the only sender clone, so the channel closes on
    // its own once it returns; this just waits for the forwarder's last
    // update to land before moving on to the next phase.
    forward_task.await.ok();

    let synthetic_base = nzb_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("release");
    let rename_report =
        penne::deobfuscate::run(&dest_dir, &queue, &outcome.assembled, synthetic_base).await?;

    let known_files: HashSet<String> = {
        let mut names: HashSet<String> = outcome.assembled.keys().cloned().collect();
        for r in &rename_report.renames {
            names.remove(&r.old_name);
            names.insert(r.new_name.clone());
        }
        names
    };

    let mut needs_repair = 0u32;
    for result in outcome.assembled.values() {
        if matches!(
            result,
            penne::assemble::AssembleOutcome::ChecksumMismatch { .. }
                | penne::assemble::AssembleOutcome::Incomplete { .. }
        ) {
            needs_repair += 1;
        }
    }

    if config.mode >= penne::config::ProcessingMode::Repair {
        set_status(state, job_id, JobStatus::Verifying).await;
        let (verify_tx, mut verify_rx) = penne::repair::channel();
        let verify_state = state.clone();
        let verify_task = tokio::spawn(async move {
            // Subtracting the interval up front lets the very first
            // `VerifyProgress` flush immediately instead of waiting out a
            // full interval before anything shows up.
            let mut last_flush = Instant::now() - FLUSH_INTERVAL;
            while let Some(vp) = verify_rx.recv().await {
                if last_flush.elapsed() >= FLUSH_INTERVAL {
                    flush_verify_progress(&verify_state, job_id, &vp).await;
                    last_flush = Instant::now();
                }
            }
        });
        let repair_outcome = penne::repair::verify_and_repair(
            &dest_dir,
            &outcome.assembled,
            &known_files,
            Some(verify_tx),
        )
        .await?;
        verify_task.await.ok();
        match repair_outcome {
            penne::repair::RepairOutcome::NotRepairable(report) => {
                anyhow::bail!(
                    "{} damaged slice(s) exceed available PAR2 recovery data ({} block(s)); download is incomplete",
                    report.total_bad_slices(),
                    report.available_recovery_blocks
                );
            }
            penne::repair::RepairOutcome::NoRecoveryData => {
                anyhow::ensure!(
                    needs_repair == 0,
                    "{needs_repair} file(s) incomplete or damaged, and no PAR2 recovery data was found to repair them"
                );
            }
            penne::repair::RepairOutcome::Ok | penne::repair::RepairOutcome::Repaired(_) => {}
        }
    } else if needs_repair > 0 {
        anyhow::bail!(
            "{needs_repair} file(s) incomplete or damaged; configure a processing mode of \
             at least \"repair\" to fix them"
        );
    }

    if config.mode >= penne::config::ProcessingMode::Unpack {
        set_status(state, job_id, JobStatus::Extracting).await;
        let password = parsed.meta.password.as_deref();
        penne::extract::extract_all(&dest_dir, password).await?;
    }

    if config.mode >= penne::config::ProcessingMode::Delete {
        penne::cleanup::purge_archives_and_par2(&dest_dir, &known_files).await?;
    }

    if config.mode >= penne::config::ProcessingMode::Repair || needs_repair == 0 {
        penne::cache::clear(&dest_dir)?;
    }

    Ok(())
}

/// Simple status transition (no byte/file data changing) — download start,
/// entering the Verifying/Extracting phases. Reuses whatever byte/speed/ETA
/// figures are already on the job so a `Progress` event still carries a
/// complete, consistent payload.
async fn set_status(state: &Arc<AppState>, job_id: Uuid, status: JobStatus) {
    let (bytes_done, total_bytes, speed_bps, eta_seconds) = {
        let mut store = state.jobs.write().await;
        let Some(active) = &mut store.active else {
            return;
        };
        if active.id != job_id {
            return;
        }
        active.status = status;
        (
            active.bytes_done,
            active.total_bytes,
            active.speed_bps,
            active.eta_seconds,
        )
    };
    state.broadcast(
        job_id,
        JobEventPayload::Progress {
            status,
            bytes_done,
            total_bytes,
            percentage: percentage(bytes_done, total_bytes),
            speed_bps,
            eta_seconds,
        },
    );
}

/// Writes the download forwarder's accumulated state to `JobStore` and
/// broadcasts it — throttled by the caller (`run_job`'s forwarder task) to
/// at most once per [`FLUSH_INTERVAL`], except for a file completing or the
/// channel closing, which always flush immediately.
///
/// `speed_bps`/`last_flush`/`last_flush_bytes` are threaded through as
/// `&mut` (owned by the forwarder task across calls) rather than read back
/// from `JobStore`: the EMA needs its own previous value every tick, and
/// storing it only in `JobStore` would mean re-reading it under the lock
/// every flush for no benefit.
async fn flush_download_progress(
    state: &Arc<AppState>,
    job_id: Uuid,
    bytes_done: u64,
    files: &HashMap<String, JobFileProgress>,
    speed_bps: &mut f64,
    last_flush: &mut Instant,
    last_flush_bytes: &mut u64,
) {
    let now = Instant::now();
    let dt = now.duration_since(*last_flush).as_secs_f64();
    if dt > 0.0 {
        let instant_speed = bytes_done.saturating_sub(*last_flush_bytes) as f64 / dt;
        *speed_bps = 0.3 * instant_speed + 0.7 * *speed_bps;
    }
    *last_flush = now;
    *last_flush_bytes = bytes_done;

    let (total_bytes, eta_seconds, pct) = {
        let mut store = state.jobs.write().await;
        let Some(active) = &mut store.active else {
            return;
        };
        if active.id != job_id {
            return;
        }
        active.status = JobStatus::Downloading;
        active.bytes_done = bytes_done;
        active.speed_bps = *speed_bps;
        active.files = files.values().cloned().collect();
        active.eta_seconds = if *speed_bps > 1.0 {
            Some((active.total_bytes.saturating_sub(bytes_done) as f64 / *speed_bps) as u64)
        } else {
            None
        };
        (active.total_bytes, active.eta_seconds, active.percentage())
    };

    state.broadcast(
        job_id,
        JobEventPayload::Progress {
            status: JobStatus::Downloading,
            bytes_done,
            total_bytes,
            percentage: pct,
            speed_bps: *speed_bps,
            eta_seconds,
        },
    );
}

/// Writes the PAR2 verify forwarder's latest [`penne::repair::VerifyProgress`]
/// to `JobStore` and broadcasts a `Progress` event so `/events/queue`
/// re-renders — the event's byte fields are the job's already-complete
/// download totals (100%), since this phase has nothing byte-shaped of its
/// own; `verify` (set here) carries the real PAR2 detail.
async fn flush_verify_progress(
    state: &Arc<AppState>,
    job_id: Uuid,
    vp: &penne::repair::VerifyProgress,
) {
    let total_bytes = {
        let mut store = state.jobs.write().await;
        let Some(active) = &mut store.active else {
            return;
        };
        if active.id != job_id {
            return;
        }
        active.verify = Some(JobVerifyProgress {
            file_name: vp.file_name.clone(),
            slices_done: vp.slices_done,
            total_slices: vp.total_slices,
        });
        active.total_bytes
    };
    state.broadcast(
        job_id,
        JobEventPayload::Progress {
            status: JobStatus::Verifying,
            bytes_done: total_bytes,
            total_bytes,
            percentage: 100.0,
            speed_bps: 0.0,
            eta_seconds: None,
        },
    );
}

fn percentage(done: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64 * 100.0).min(100.0)
    }
}
