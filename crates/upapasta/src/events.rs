#![allow(dead_code)]

use crossterm::event::KeyEvent;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

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
}

#[derive(Debug, Clone)]
pub struct FileProgressUpdate {
    pub name: String,
    pub done_segments: u64,
    pub total_segments: u64,
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub ok: bool, // for SegmentDone
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
    // Internal: upload task finished
    UploadFinished { success: bool, cancelled: bool },
    // Upload control
    PauseUpload,
    ResumeUpload,
    Quit,
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

// Helper to create a channel for pesto progress
pub fn create_progress_channel() -> (UnboundedSender<String>, UnboundedReceiver<String>) {
    mpsc::unbounded_channel()
}
