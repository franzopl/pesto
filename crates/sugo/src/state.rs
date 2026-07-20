//! Shared application state: config, job store, and the one global
//! broadcast channel SSE handlers subscribe to and filter by job id (see
//! the plan's Design Decision 3 — avoids a `HashMap<JobId, Sender>`
//! bookkeeping layer for the MVP).

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

use crate::config::WebConfig;
use crate::job::{JobStatus, JobStore};

pub struct AppState {
    pub config: RwLock<WebConfig>,
    /// Where the config file was loaded from, if any — [`crate::web::settings`]
    /// writes edits back here. `None` when running without a config file on
    /// disk at all (nothing to persist server edits to yet).
    pub config_path: Option<PathBuf>,
    pub jobs: RwLock<JobStore>,
    pub events: broadcast::Sender<JobEvent>,
    pub data_dir: PathBuf,
}

pub type SharedState = Arc<AppState>;

/// One update about a job, broadcast to every SSE subscriber; handlers
/// filter down to the job(s) they care about by `job_id`.
#[derive(Debug, Clone, Serialize)]
pub struct JobEvent {
    pub job_id: Uuid,
    #[serde(flatten)]
    pub payload: JobEventPayload,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum JobEventPayload {
    Progress {
        status: JobStatus,
        bytes_done: u64,
        total_bytes: u64,
        percentage: f64,
        speed_bps: f64,
        eta_seconds: Option<u64>,
    },
    Finished {
        status: JobStatus,
        name: String,
    },
}

impl AppState {
    pub fn new(config: WebConfig, data_dir: PathBuf, config_path: Option<PathBuf>) -> SharedState {
        let state_path = data_dir.join("state.json");
        let jobs = JobStore::load_or_default(state_path);
        let (events, _rx) = broadcast::channel(1024);
        Arc::new(AppState {
            config: RwLock::new(config),
            config_path,
            jobs: RwLock::new(jobs),
            events,
            data_dir,
        })
    }

    /// Best-effort: no subscribers is the common case (no browser tab open),
    /// not an error.
    pub fn broadcast(&self, job_id: Uuid, payload: JobEventPayload) {
        let _ = self.events.send(JobEvent { job_id, payload });
    }

    /// Announces a freshly staged job so `/events/queue` re-renders and
    /// shows it right away, instead of waiting for the worker to actually
    /// pick it up (which may be a while behind a busy queue) before the
    /// dashboard reflects the upload at all.
    pub fn broadcast_queued(&self, job_id: Uuid, total_bytes: u64) {
        self.broadcast(
            job_id,
            JobEventPayload::Progress {
                status: JobStatus::Queued,
                bytes_done: 0,
                total_bytes,
                percentage: 0.0,
                speed_bps: 0.0,
                eta_seconds: None,
            },
        );
    }
}
