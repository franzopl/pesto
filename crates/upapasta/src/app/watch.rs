//! State for the Watch screen.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::{
    expand_tilde, fold_watch_scan, watch_settings_path, App, AppState, FileProgress, FileStatus,
    UploadProgress,
};

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

impl App {
    pub fn watch_select_next(&mut self) {
        self.watch.selected = (self.watch.selected + 1).min(WATCH_FIELD_COUNT - 1);
    }

    pub fn watch_select_prev(&mut self) {
        self.watch.selected = self.watch.selected.saturating_sub(1);
    }

    pub fn watch_start_edit(&mut self) {
        self.watch.edit_buf = match self.watch.selected {
            0 => self
                .watch
                .dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            1 => self
                .watch
                .done_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            2 => self.watch.ext_filter.clone(),
            3 => self.watch.interval_secs.to_string(),
            _ => String::new(),
        };
        self.watch.editing = true;
    }

    pub fn watch_cancel_edit(&mut self) {
        self.watch.editing = false;
        self.watch.edit_buf.clear();
    }

    /// Commit the edit buffer for the selected field. Changing the watched
    /// directory resets all stability/dedup state: paths tracked against the
    /// old directory have no bearing on the new one.
    pub fn watch_confirm_edit(&mut self) {
        let buf = self.watch.edit_buf.trim().to_string();
        match self.watch.selected {
            0 => {
                let new_dir = if buf.is_empty() {
                    None
                } else {
                    Some(expand_tilde(&buf))
                };
                if new_dir != self.watch.dir {
                    self.watch.dir = new_dir;
                    self.watch.pending.clear();
                    self.watch.ready.clear();
                    self.watch.seen.clear();
                    self.watch.baseline_captured = false;
                    self.watch.last_scan = None;
                }
            }
            1 => {
                self.watch.done_dir = if buf.is_empty() {
                    None
                } else {
                    Some(expand_tilde(&buf))
                };
            }
            2 => {
                self.watch.ext_filter = buf.trim_start_matches('.').replace(' ', "");
            }
            3 => {
                if let Ok(secs) = buf.parse::<u64>() {
                    self.watch.interval_secs = secs.max(5);
                }
            }
            _ => {}
        }
        self.watch.editing = false;
        self.watch.edit_buf.clear();
        self.save_watch_settings();
    }

    /// Start or stop watching. Refuses to start without a valid directory;
    /// stopping never touches items already queued or mid-upload — it only
    /// stops future scans/dispatches.
    pub fn toggle_watch_enabled(&mut self) {
        if self.watch.enabled {
            self.watch.enabled = false;
            self.status_bar.set("Watch stopped");
            self.log_panel.push("=== Watch stopped ===".to_string());
            return;
        }
        let Some(dir) = self.watch.dir.clone() else {
            self.status_bar.set("Set a directory first (Enter to edit)");
            return;
        };
        if !dir.is_dir() {
            self.status_bar
                .set(format!("Not a directory: {}", dir.display()));
            return;
        }
        self.watch.enabled = true;
        self.watch.last_scan = None; // scan on the next loop iteration
        self.status_bar.set(format!("Watching {}", dir.display()));
        self.log_panel
            .push(format!("=== Watch started: {} ===", dir.display()));
    }

    /// Fold a background scan's (path, size) snapshot into the stability
    /// tracker and log the result. The actual bookkeeping lives in the
    /// free function [`fold_watch_scan`] so it is testable without a full
    /// `App`.
    pub fn apply_watch_scan(&mut self, entries: Vec<(PathBuf, u64)>) {
        self.watch.scanning = false;
        self.watch.last_scan = Some(Instant::now());
        for (line, is_warn) in fold_watch_scan(&mut self.watch, &entries) {
            if is_warn {
                self.log_panel.push_warn(line);
            } else {
                self.log_panel.push(line);
            }
        }
    }

    /// Prepare app state for a single watch-triggered upload and return the
    /// pieces the caller needs to spawn the pipeline. Mirrors `trigger_upload`
    /// but for exactly one item, outside the manual queue.
    pub fn begin_watch_upload(
        &mut self,
        path: &Path,
    ) -> (String, CancellationToken, Arc<AtomicBool>) {
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "watch-item".to_string());

        self.watch.current = Some(path.to_path_buf());
        self.upload_in_progress = true;
        self.upload_started_at = Some(Instant::now());

        // Unlike the manual queue's `y`-to-confirm flow (which already jumps
        // to the Dashboard as part of that explicit action), a watch upload
        // starts on its own — surface it unless the user is mid-edit
        // somewhere else, where yanking the screen away would lose context.
        if !self.watch.editing
            && !self.config_state.editing
            && !self.confirm_editing
            && !self.log_panel.searching
            && !self.history.searching
        {
            self.state = AppState::Dashboard;
        }

        let token = CancellationToken::new();
        self.current_cancel_token = Some(token.clone());
        let pause_flag = Arc::new(AtomicBool::new(false));
        self.current_pause_flag = Some(pause_flag.clone());

        self.progress = UploadProgress {
            start_time: Some(Instant::now()),
            speed_history: vec![0.0; 5],
            files: vec![FileProgress {
                name: label.clone(),
                total_segments: 0,
                done_segments: 0,
                total_bytes: 0,
                done_bytes: 0,
                status: FileStatus::Pending,
            }],
            ..Default::default()
        };

        self.status_bar
            .set(format!("[watch] uploading {label} (x to cancel)"));
        self.log_panel
            .push(format!("=== [watch] Starting upload: {label} ==="));

        (label, token, pause_flag)
    }

    /// A watch-triggered upload finished. Unlike the manual queue's
    /// `upload_finished`, a failed or cancelled item is never retried here —
    /// it stays in `watch.seen` permanently, matching the manual `x`-to-cancel
    /// semantics used everywhere else in the app.
    pub fn watch_upload_done(
        &mut self,
        path: PathBuf,
        success: bool,
        cancelled: bool,
        size_bytes: u64,
        nzb_path: Option<PathBuf>,
        duration_s: f64,
    ) {
        self.upload_in_progress = false;
        self.watch.current = None;
        self.current_cancel_token = None;
        self.current_pause_flag = None;
        self.progress.is_paused = false;

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());

        if cancelled {
            self.progress.is_cancelled = true;
            self.status_bar.set(format!("[watch] cancelled: {name}"));
            self.log_panel
                .push(format!("=== [watch] Cancelled: {name} ==="));
            return;
        }
        self.progress.is_cancelled = false;

        self.record_catalog_entry(name.clone(), size_bytes, nzb_path, duration_s, !success);

        if success {
            self.status_bar.set(format!("[watch] done: {name}"));
            self.log_panel
                .push(format!("=== [watch] Completed: {name} ==="));
            self.move_watch_item_to_done(&path, &name);
        } else {
            self.status_bar
                .set(format!("[watch] failed: {name} (see log)"));
            self.log_panel
                .push_error(format!("[watch] upload failed: {name}"));
        }
    }

    /// Move a successfully uploaded watch item into `watch.done_dir`, if set.
    /// `rename` is a same-filesystem metadata op (no data copy), so this is
    /// safe to call inline from the event loop; a cross-device destination
    /// fails fast rather than silently copying gigabytes on the render thread.
    fn move_watch_item_to_done(&mut self, path: &Path, name: &str) {
        let Some(done_dir) = self.watch.done_dir.clone() else {
            return;
        };
        if let Err(e) = std::fs::create_dir_all(&done_dir) {
            self.log_panel
                .push_error(format!("[watch] could not create done dir: {e}"));
            return;
        }
        let Some(file_name) = path.file_name() else {
            return;
        };
        let dest = done_dir.join(file_name);
        match std::fs::rename(path, &dest) {
            Ok(()) => {
                self.log_panel
                    .push(format!("[watch] moved {name} -> {}", dest.display()));
            }
            Err(e) => {
                self.log_panel
                    .push_error(format!("[watch] move failed for {name}: {e}"));
            }
        }
    }

    /// Persist watch settings (directory, done dir, extension filter,
    /// interval) so they survive a restart. `enabled` is deliberately not
    /// persisted — see `WatchSettings`'s doc comment.
    pub fn save_watch_settings(&self) {
        if let Some(path) = watch_settings_path() {
            let settings = WatchSettings {
                dir: self.watch.dir.clone(),
                done_dir: self.watch.done_dir.clone(),
                ext_filter: self.watch.ext_filter.clone(),
                interval_secs: self.watch.interval_secs,
            };
            if let Ok(json) = serde_json::to_string_pretty(&settings) {
                let _ = std::fs::write(path, json);
            }
        }
    }

    /// Load previously saved watch settings. Falls back to a 30s interval
    /// when nothing was ever saved (or the saved value is unusable).
    pub fn load_watch_settings(&mut self) {
        let loaded = watch_settings_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|data| serde_json::from_str::<WatchSettings>(&data).ok());
        let settings = loaded.unwrap_or_default();
        self.watch.dir = settings.dir;
        self.watch.done_dir = settings.done_dir;
        self.watch.ext_filter = settings.ext_filter;
        self.watch.interval_secs = if settings.interval_secs == 0 {
            30
        } else {
            settings.interval_secs
        };
    }
}
