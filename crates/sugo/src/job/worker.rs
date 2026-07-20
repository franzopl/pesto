//! Background job consumer: pulls one job at a time off the pending queue
//! and runs it through [`super::pipeline::run_job`] to completion before
//! picking up the next — see the plan's note on why sequential processing
//! is the right MVP concurrency model (`penne`'s `DownloadClient` isn't
//! pooled across runs, so nothing is gained by racing two jobs' NNTP
//! connections against each other yet).

use std::sync::Arc;
use std::time::Duration;

use crate::job::JobStatus;
use crate::state::{AppState, JobEventPayload};

/// Poll interval when the queue is empty or paused.
const IDLE_POLL: Duration = Duration::from_millis(500);

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            let next = { state.jobs.write().await.try_start_next() };
            let Some(job) = next else {
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            };

            let job_id = job.id;
            tracing::info!(name = %job.name, %job_id, "starting job");
            let result = super::pipeline::run_job(&state, job_id).await;

            let mut store = state.jobs.write().await;
            let Some(mut active) = store.active.take() else {
                // Shouldn't happen: only this worker ever clears `active`.
                continue;
            };
            active.status = match &result {
                Ok(()) => JobStatus::Completed,
                Err(e) => {
                    tracing::error!(%job_id, error = %e, "job failed");
                    active.message = Some(e.to_string());
                    JobStatus::Failed
                }
            };
            active.finished_at = Some(crate::job::now_millis());
            let status = active.status;
            store.finish_active(active);
            let _ = store.save();
            drop(store);

            state.broadcast(job_id, JobEventPayload::Finished { status });
        }
    });
}
