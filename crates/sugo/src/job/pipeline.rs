//! Per-job port of `penne`'s own CLI pipeline
//! (`crates/penne/src/bin/penne.rs`'s `download()`): nzb load -> queue build
//! -> disk-space check -> `download_queue` -> deobfuscate -> (mode-gated)
//! PAR2 verify/repair -> extract -> cleanup -> cache clear.
//!
//! The only structural difference from the CLI: every `penne::ui::*::
//! spawn_renderer(rx)` call (which draws a terminal panel) is replaced here
//! by a task that updates this job's live byte counter in [`AppState`] and
//! broadcasts a [`JobEventPayload::Progress`] over SSE instead.
//!
//! PAR2 verify and archive extraction don't get their own live percentage
//! here (unlike the CLI's PAR2 panel) — only a status transition
//! (`Verifying`/`Extracting`). That progress channel exists
//! ([`penne::repair::VerifyProgress`]) and forwarding it is a natural
//! follow-up, but it's deliberately out of scope for this first vertical
//! slice: the queue/history view (this crate's main payoff) doesn't need it
//! to be useful.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};
use uuid::Uuid;

use crate::job::JobStatus;
use crate::state::{AppState, JobEventPayload};

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
        let mut bytes_done: u64 = 0;
        while let Some(event) = rx.recv().await {
            if let penne::progress::ProgressEvent::SegmentDownloaded { bytes, .. } = event {
                bytes_done += bytes;
                update_progress(&forward_state, job_id, bytes_done, JobStatus::Downloading).await;
            }
        }
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
        let drain_task = tokio::spawn(async move { while verify_rx.recv().await.is_some() {} });
        let repair_outcome = penne::repair::verify_and_repair(
            &dest_dir,
            &outcome.assembled,
            &known_files,
            Some(verify_tx),
        )
        .await?;
        drain_task.await.ok();
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

async fn set_status(state: &Arc<AppState>, job_id: Uuid, status: JobStatus) {
    let (bytes_done, total_bytes) = {
        let mut store = state.jobs.write().await;
        if let Some(active) = &mut store.active {
            if active.id == job_id {
                active.status = status;
            }
        }
        store
            .active
            .as_ref()
            .map(|j| (j.bytes_done, j.total_bytes))
            .unwrap_or((0, 0))
    };
    state.broadcast(
        job_id,
        JobEventPayload::Progress {
            status,
            bytes_done,
            total_bytes,
            percentage: percentage(bytes_done, total_bytes),
        },
    );
}

async fn update_progress(state: &Arc<AppState>, job_id: Uuid, bytes_done: u64, status: JobStatus) {
    let total_bytes = {
        let mut store = state.jobs.write().await;
        if let Some(active) = &mut store.active {
            if active.id == job_id {
                active.bytes_done = bytes_done;
                active.status = status;
            }
        }
        store.active.as_ref().map(|j| j.total_bytes).unwrap_or(0)
    };
    state.broadcast(
        job_id,
        JobEventPayload::Progress {
            status,
            bytes_done,
            total_bytes,
            percentage: percentage(bytes_done, total_bytes),
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
