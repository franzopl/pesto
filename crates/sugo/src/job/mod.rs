//! Job model and in-memory store, persisted to a JSON snapshot so the queue
//! and history survive a restart (no `sled`/`rusqlite` yet — unjustified
//! weight for an MVP; see the plan's Design Decision 2).
//!
//! Deliberately its own small, hand-built type rather than a `Serialize`d
//! `penne::download::DownloadOutcome` or similar: those describe one run's
//! internal bookkeeping, not the SABnzbd-shaped queue/history entry a web
//! frontend needs — see the plan's Design Decision 1.

pub mod pipeline;
pub mod worker;

use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::AppState;

/// Where a job currently stands, mirroring `sabnzbd`'s own per-item status
/// vocabulary closely enough for `*arr` tooling to recognize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Downloading,
    Verifying,
    Extracting,
    Completed,
    Failed,
}

impl JobStatus {
    pub fn sabnzbd_label(self) -> &'static str {
        match self {
            JobStatus::Queued => "Queued",
            JobStatus::Downloading => "Downloading",
            JobStatus::Verifying => "Verifying",
            JobStatus::Extracting => "Extracting",
            JobStatus::Completed => "Completed",
            JobStatus::Failed => "Failed",
        }
    }

    /// Lowercase, CSS-class-friendly form of [`Self::sabnzbd_label`] (e.g.
    /// `status-downloading`), computed here rather than with an askama
    /// template filter — one obviously-correct Rust match beats depending
    /// on a filter's exact casing behavior.
    pub fn css_class(self) -> &'static str {
        match self {
            JobStatus::Queued => "status-queued",
            JobStatus::Downloading => "status-downloading",
            JobStatus::Verifying => "status-verifying",
            JobStatus::Extracting => "status-extracting",
            JobStatus::Completed => "status-completed",
            JobStatus::Failed => "status-failed",
        }
    }
}

/// One file in the release, tracked incrementally as its segments arrive —
/// seeded from `penne::progress::ProgressEvent::Started`, updated by
/// `SegmentDownloaded`/`FileAssembled`. Lets the UI show per-file progress,
/// not just one job-wide bar.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct JobFileProgress {
    pub name: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub done: bool,
}

/// The PAR2 verify pass's live position, from
/// `penne::repair::VerifyProgress` — scoped to whichever file is
/// *currently* being verified, not a release-wide total (there's no cheap
/// way to know the grand total up front without loading the whole recovery
/// set ourselves; see the plan's "known, accepted approximation").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobVerifyProgress {
    pub file_name: String,
    pub slices_done: usize,
    pub total_slices: usize,
}

/// One NZB, queued/running/finished. Doubles as both the "active queue
/// slot" and "history entry" shape — the same fields SABnzbd's own
/// `mode=queue`/`mode=history` slots carry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub name: String,
    pub category: String,
    pub status: JobStatus,
    pub total_bytes: u64,
    pub bytes_done: u64,
    /// Bytes/second, a simple EMA over the download forwarder's throttled
    /// flushes (`job::pipeline`). `0.0` when unknown (not yet downloading,
    /// or between flushes).
    #[serde(default)]
    pub speed_bps: f64,
    /// `None` when speed is `0.0` (nothing to divide by) or the job isn't
    /// downloading.
    #[serde(default)]
    pub eta_seconds: Option<u64>,
    #[serde(default)]
    pub files: Vec<JobFileProgress>,
    #[serde(default)]
    pub verify: Option<JobVerifyProgress>,
    pub dest_dir: PathBuf,
    /// Where the uploaded/fetched `.nzb` was staged
    /// (`<data_dir>/jobs/<id>.nzb`).
    pub nzb_path: PathBuf,
    pub submitted_at: u64,
    pub finished_at: Option<u64>,
    /// Failure reason once `status == Failed`; unset otherwise.
    pub message: Option<String>,
}

impl Job {
    pub fn percentage(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            (self.bytes_done as f64 / self.total_bytes as f64 * 100.0).min(100.0)
        }
    }
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Writes an uploaded/fetched `.nzb`'s bytes to this instance's data
/// directory and builds a fresh, still-`Queued` [`Job`] for it — shared by
/// both `mode=addfile` (multipart upload) and `mode=addurl` (fetched via
/// `reqwest`).
pub async fn stage_and_create(
    state: &AppState,
    filename: &str,
    category: Option<String>,
    bytes: Vec<u8>,
) -> Result<Job> {
    let id = Uuid::new_v4();
    let jobs_dir = state.data_dir.join("jobs");
    tokio::fs::create_dir_all(&jobs_dir)
        .await
        .with_context(|| format!("creating {}", jobs_dir.display()))?;
    let nzb_path = jobs_dir.join(format!("{id}.nzb"));
    tokio::fs::write(&nzb_path, &bytes)
        .await
        .with_context(|| format!("writing {}", nzb_path.display()))?;

    let parsed = penne::nzb::load(&nzb_path).context("parsing uploaded .nzb")?;
    let summary = penne::nzb::summarize(&parsed);

    let name = std::path::Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("release")
        .to_string();

    let category = category
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "*".to_string());
    let base_dir = {
        let config = state.config.read().await;
        let category_dir = config
            .web
            .categories
            .iter()
            .find(|c| c.name == category)
            .and_then(|c| c.dir.clone());
        match category_dir {
            Some(dir) => PathBuf::from(dir),
            None => config
                .core
                .download_dir
                .clone()
                .unwrap_or_else(|| state.data_dir.join("downloads")),
        }
    };
    let dest_dir = base_dir.join(&name);

    Ok(Job {
        id,
        name,
        category,
        status: JobStatus::Queued,
        total_bytes: summary.total_bytes,
        bytes_done: 0,
        speed_bps: 0.0,
        eta_seconds: None,
        files: Vec::new(),
        verify: None,
        dest_dir,
        nzb_path,
        submitted_at: now_millis(),
        finished_at: None,
        message: None,
    })
}

/// Pending/active/history queue, persisted as one JSON snapshot.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct JobStore {
    pub pending: VecDeque<Job>,
    pub active: Option<Job>,
    pub history: Vec<Job>,
    pub paused: bool,
    #[serde(skip)]
    state_path: PathBuf,
}

impl JobStore {
    /// Load a previous snapshot from `state_path`, or start empty if none
    /// exists (first run, or the file is missing/corrupt).
    pub fn load_or_default(state_path: PathBuf) -> Self {
        let mut store = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|contents| serde_json::from_str::<JobStore>(&contents).ok())
            .unwrap_or_default();
        store.state_path = state_path;
        store
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).context("serializing job state")?;
        std::fs::write(&self.state_path, json)
            .with_context(|| format!("writing {}", self.state_path.display()))
    }

    pub fn enqueue(&mut self, job: Job) {
        self.pending.push_back(job);
    }

    /// Pop the next pending job into `active`, unless a job is already
    /// running or the queue is paused. One active job at a time — see the
    /// plan's Design Decision on sequential processing (`penne`'s
    /// `DownloadClient` isn't pooled across runs, so there's nothing to gain
    /// from more concurrency here yet).
    pub fn try_start_next(&mut self) -> Option<Job> {
        if self.paused || self.active.is_some() {
            return None;
        }
        let job = self.pending.pop_front()?;
        self.active = Some(job.clone());
        Some(job)
    }

    pub fn finish_active(&mut self, job: Job) {
        self.active = None;
        self.history.insert(0, job);
    }

    pub fn remove_pending(&mut self, id: Uuid) -> bool {
        let before = self.pending.len();
        self.pending.retain(|j| j.id != id);
        self.pending.len() != before
    }

    pub fn remove_history(&mut self, id: Uuid) -> bool {
        let before = self.history.len();
        self.history.retain(|j| j.id != id);
        self.history.len() != before
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_nzb_bytes() -> Vec<u8> {
        let groups = vec!["alt.binaries.test".to_string()];
        let segments = vec![pesto::poster::PostedSegment {
            file_name: "movie.bin".into(),
            file_path: "movie.bin".into(),
            subject_name: "movie.bin".into(),
            file_size: 10,
            part: 1,
            total: 1,
            message_id: "<art1@test>".into(),
            bytes: 10,
            from: "poster <p@x>".into(),
            date: (None, None),
            full_crc32: 0,
            server_idx: 0,
        }];
        pesto::nzb::generate(&groups, &segments, &pesto::nzb::NzbMeta::default()).into_bytes()
    }

    #[tokio::test]
    async fn stage_and_create_resolves_dest_dir_from_a_matching_category() {
        let dir = tempfile::tempdir().unwrap();
        let category_dir = dir.path().join("movies-dir");
        let mut config = crate::config::WebConfig::default();
        config.web.categories.push(crate::config::CategoryConfig {
            name: "movies".to_string(),
            dir: Some(category_dir.to_string_lossy().to_string()),
        });
        let state = crate::state::AppState::new(config, dir.path().join("data"), None);

        let job = stage_and_create(
            &state,
            "release.nzb",
            Some("movies".to_string()),
            minimal_nzb_bytes(),
        )
        .await
        .unwrap();
        assert_eq!(job.dest_dir, category_dir.join("release"));
    }

    #[tokio::test]
    async fn stage_and_create_falls_back_to_download_dir_for_an_unconfigured_category() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::WebConfig::default();
        config.core.download_dir = Some(dir.path().join("downloads"));
        let state = crate::state::AppState::new(config, dir.path().join("data"), None);

        let job = stage_and_create(
            &state,
            "release.nzb",
            Some("tv".to_string()),
            minimal_nzb_bytes(),
        )
        .await
        .unwrap();
        assert_eq!(job.dest_dir, dir.path().join("downloads").join("release"));
        assert_eq!(job.category, "tv");
    }

    fn dummy_job(name: &str) -> Job {
        Job {
            id: Uuid::new_v4(),
            name: name.to_string(),
            category: "*".to_string(),
            status: JobStatus::Queued,
            total_bytes: 100,
            bytes_done: 0,
            speed_bps: 0.0,
            eta_seconds: None,
            files: Vec::new(),
            verify: None,
            dest_dir: PathBuf::from("/tmp/dest"),
            nzb_path: PathBuf::from("/tmp/job.nzb"),
            submitted_at: now_millis(),
            finished_at: None,
            message: None,
        }
    }

    #[test]
    fn try_start_next_respects_pause_and_one_active_slot() {
        let mut store = JobStore::default();
        store.enqueue(dummy_job("a"));
        store.enqueue(dummy_job("b"));

        store.paused = true;
        assert!(store.try_start_next().is_none());

        store.paused = false;
        let started = store.try_start_next().unwrap();
        assert_eq!(started.name, "a");
        assert!(store.active.is_some());
        // Already busy: the second pending job must wait.
        assert!(store.try_start_next().is_none());
    }

    #[test]
    fn finish_active_moves_the_job_to_history_head() {
        let mut store = JobStore::default();
        store.enqueue(dummy_job("a"));
        let job = store.try_start_next().unwrap();
        store.finish_active(job);
        assert!(store.active.is_none());
        assert_eq!(store.history.len(), 1);
        assert_eq!(store.history[0].name, "a");
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let mut store = JobStore::default();
        store.enqueue(dummy_job("a"));
        let json = serde_json::to_string(&store).unwrap();
        let restored: JobStore = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pending.len(), 1);
        assert_eq!(restored.pending[0].name, "a");
    }
}
