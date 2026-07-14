#![allow(dead_code)]

use crossterm::event::KeyEvent;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// High-level phase of the pipeline.
///
/// PAR2 encoding and NNTP posting run concurrently inside pesto; the phase
/// only switches to distinct states for the sequential steps (compress,
/// write par2 volumes, verify). During `Uploading`, PAR2 progress is tracked
/// separately in `UploadProgress::par2_{done,total}_slices`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UploadPhase {
    #[default]
    Preparing,
    Compressing {
        done_bytes: u64,
        total_bytes: u64,
    },
    /// NNTP posting (+ concurrent PAR2 encoding). Main phase.
    Uploading,
    /// Writing computed PAR2 recovery volumes to disk (sequential, brief).
    WritingPar2 {
        written: u32,
        total: u32,
    },
    Done,
}

/// Structured progress information extracted from pesto::progress::ProgressEvent
#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub done_segments: u64,
    pub total_segments: u64,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub current_speed_mbps: f64,
    pub message: Option<String>,

    /// Optional per-file update (when a specific file advanced)
    pub file_update: Option<FileProgressUpdate>,

    /// Current pipeline phase (None = no change from previous)
    pub phase: Option<UploadPhase>,

    /// PAR2 encoding progress (concurrent with NNTP posting); None = no change
    pub par2_slices: Option<(usize, usize)>, // (done, total)

    /// Streaming check queue progress (concurrent with NNTP posting, runs
    /// for the lifetime of the upload rather than as its own phase);
    /// `(checked, failed)`. None = no change.
    pub check_progress: Option<(u64, u64)>,

    /// When set, this update extends the queue by the given bytes/segments.
    /// apply() absorbs it against par2_hint_remaining instead of blindly adding.
    pub queue_extended: Option<(u64, u64)>, // (segments, bytes)

    /// PAR2 bytes pre-seeded into total_bytes from par2_bytes_hint (Started event only).
    /// apply() stores it so QueueExtended can absorb against it.
    pub par2_hint_bytes: u64,

    /// PAR2 segments pre-seeded into total_segments from par2_segments_hint
    /// (Started event only), mirroring par2_hint_bytes — without this the
    /// segment-based progress percentage jumps once PAR2 volumes are queued.
    pub par2_segment_hint: u64,

    /// When true, PAR2 encode+write is fully complete. apply() marks par2_finished.
    pub par2_complete: bool,
}

#[derive(Debug, Clone)]
pub struct FileProgressUpdate {
    pub name: String,
    pub done_segments: u64,
    pub total_segments: u64,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub enum AppEvent {
    // Human readable log line
    Progress(String),
    // Structured progress for accurate bars / stats
    ProgressUpdate(ProgressUpdate),
    // Keyboard (routed from the async EventStream)
    Key(KeyEvent),
    // File selection events
    FileSelected(String),
    // Upload lifecycle
    UploadStarted,
    UploadCompleted,
    UploadError(String),
    // Periodic UI tick
    Tick,
    // A background directory scan finished: per-item (path, backed, size). The
    // generation lets the FileTree drop results for a directory it already left.
    DirScanReady {
        generation: u64,
        results: Vec<(std::path::PathBuf, bool, u64)>,
    },
    // A queued folder's recursive size walk finished: (path, file_count, bytes).
    QueueMetaReady {
        key: String,
        file_count: usize,
        size_bytes: u64,
    },
    // A single queue item started uploading (sequential, one NZB at a time).
    ItemUploadStarted {
        path: String,
    },
    // The files of the current pesto run, taken from its `Started` event. Each
    // tuple is `(real_name, total_segments, total_bytes)`. Used to seed the
    // per-file progress rows keyed by the same `real_name` that later
    // `SegmentDone` events carry, so per-episode bars actually advance (folder
    // queue entries upload each inner file under its own name, which never
    // matched the folder-keyed rows before).
    RegisterFiles {
        files: Vec<(String, u64, u64)>,
    },
    // A single queue item finished, carrying the real data pesto produced so
    // the catalog can be written per-item (real size, real NZB path) instead of
    // a fabricated average at the end of the batch.
    //
    // `record_catalog` is false when the item produced several NZBs (per-file /
    // season folder modes): each NZB is then recorded via its own
    // `CatalogRecord`, so the item-level event only updates the queue status.
    ItemUploadDone {
        path: String,
        success: bool,
        size_bytes: u64,
        nzb_path: Option<std::path::PathBuf>,
        duration_s: f64,
        record_catalog: bool,
    },
    // Record one produced NZB in the catalog (used by per-file / season folder
    // modes where a single queue entry yields multiple NZBs).
    CatalogRecord {
        original_name: String,
        size_bytes: u64,
        nzb_path: Option<std::path::PathBuf>,
        duration_s: f64,
    },
    // Internal: upload task finished
    UploadFinished {
        success: bool,
        cancelled: bool,
    },
    // Upload control
    Quit,
    // Prowlarr connection check result
    ProwlarrStatus(crate::prowlarr::ConnectionStatus),
    // Prowlarr search results (Ok) or error (Err)
    ProwlarrSearchDone(Result<Vec<crate::prowlarr::SearchResult>, String>),
    // NZB download finished: Ok(dest_path) or Err(msg)
    ProwlarrDownloadDone(Result<std::path::PathBuf, String>),
    // One item of a queue batch search finished. `outcome` is a human-readable
    // log line; the counters carry the running tally for the progress display.
    ProwlarrBatchProgress {
        done: usize,
        total: usize,
        current: String,
        downloaded: usize,
        no_match: usize,
        failed: usize,
        log: String,
    },
    // Queue batch search finished with final tallies.
    ProwlarrBatchDone {
        downloaded: usize,
        no_match: usize,
        failed: usize,
    },
    // Manual "run a hook on the selected release" finished. `ok` is true only
    // when the hook exited 0; the app then records the run for the release so
    // the Browser and picker can flag it. `release_key`/`hook_name` are empty
    // for early failures (e.g. no .nzb found), which are not recorded.
    HooksDone {
        ok: bool,
        release_key: String,
        release_name: String,
        hook_name: String,
        log: Vec<String>,
    },
}

pub struct EventHandler {
    pub tx: UnboundedSender<AppEvent>,
    pub rx: UnboundedReceiver<AppEvent>,
}

impl EventHandler {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { tx, rx }
    }

    pub fn send(&self, event: AppEvent) {
        let _ = self.tx.send(event);
    }
}

pub fn create_progress_channel() -> (UnboundedSender<String>, UnboundedReceiver<String>) {
    mpsc::unbounded_channel()
}
