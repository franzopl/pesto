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
    Verifying {
        checked: u64,
        total: u64,
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

    /// When set, this update extends the queue by the given bytes/segments.
    /// apply() absorbs it against par2_hint_remaining instead of blindly adding.
    pub queue_extended: Option<(u64, u64)>, // (segments, bytes)

    /// PAR2 bytes pre-seeded into total_bytes from par2_bytes_hint (Started event only).
    /// apply() stores it so QueueExtended can absorb against it.
    pub par2_hint_bytes: u64,

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
    // A single queue item started uploading (sequential, one NZB at a time).
    ItemUploadStarted {
        path: String,
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
