//! State for the Watch screen.

use std::path::PathBuf;
use std::time::Instant;

/// Persisted watch-mode settings (survives restarts); `enabled` is
/// deliberately excluded — watch never resumes silently on launch, so a
/// background upload can never start without the user seeing it happen.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct WatchSettings {
    pub(super) dir: Option<PathBuf>,
    pub(super) done_dir: Option<PathBuf>,
    /// Comma-separated extensions (e.g. "mkv,mp4"); empty = no filtering.
    pub(super) ext_filter: String,
    pub(super) interval_secs: u64,
}

/// State for the Watch screen: monitors one directory, auto-uploading each
/// top-level entry once its size is unchanged across two consecutive scans
/// (the same settle check `pesto --watch` uses), then optionally moves the
/// source into `done_dir`.
#[derive(Debug, Default)]
pub struct WatchState {
    pub enabled: bool,
    pub dir: Option<PathBuf>,
    pub done_dir: Option<PathBuf>,
    pub ext_filter: String,
    pub interval_secs: u64,
    pub last_scan: Option<Instant>,
    /// Set while a background scan of `dir` is in flight, so the poll loop
    /// never overlaps two scans of the same directory.
    pub scanning: bool,
    /// True once the first scan of the current `dir` has run: that scan's
    /// entries are the pre-existing baseline (ignored, never uploaded) —
    /// only entries that show up in later scans are new arrivals.
    pub baseline_captured: bool,
    /// path -> size observed on the previous scan, for the settle check.
    pub pending: std::collections::HashMap<PathBuf, u64>,
    /// Stable entries waiting for the poster to be free.
    pub ready: std::collections::VecDeque<PathBuf>,
    /// Permanently resolved paths (baselined, uploaded, failed, cancelled or
    /// ignored as empty) that must never be re-detected.
    pub seen: std::collections::HashSet<PathBuf>,
    /// The entry currently uploading via watch, if any.
    pub current: Option<PathBuf>,
    /// Index of the selected field in the Watch screen's field list.
    pub selected: usize,
    pub editing: bool,
    pub edit_buf: String,
}

pub const WATCH_FIELD_COUNT: usize = 4;
