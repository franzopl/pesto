use crate::events::ProgressUpdate;
use crate::ui::components::{FileTree, LogPanel, StatusBar, UploadQueue};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AppState {
    #[default]
    Dashboard,
    Browser,
    History,
    Config,
}

#[derive(Debug, Default)]
pub struct UploadProgress {
    pub total_segments: u64,
    pub done_segments: u64,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub start_time: Option<Instant>,
    pub last_speed: f64, // MB/s
    #[allow(dead_code)]
    pub active_connections: usize,
    pub is_cancelled: bool,
}

impl UploadProgress {
    pub fn progress_pct(&self) -> f64 {
        if self.total_segments == 0 {
            return 0.0;
        }
        (self.done_segments as f64 / self.total_segments as f64 * 100.0).min(100.0)
    }

    pub fn eta_seconds(&self) -> Option<u64> {
        if self.last_speed <= 0.0 || self.total_bytes == 0 {
            return None;
        }
        let remaining = self.total_bytes.saturating_sub(self.done_bytes);
        let mb_remaining = remaining as f64 / (1024.0 * 1024.0);
        Some((mb_remaining / self.last_speed) as u64)
    }

    pub fn apply(&mut self, update: &ProgressUpdate) {
        if update.total_segments > 0 {
            self.total_segments = update.total_segments;
        }
        if update.total_bytes > 0 {
            self.total_bytes = update.total_bytes;
        }
        if update.done_segments > self.done_segments {
            self.done_segments = update.done_segments;
        }
        if update.done_bytes > self.done_bytes {
            self.done_bytes = update.done_bytes;
        }
        if update.current_speed_mbps > 0.0 {
            self.last_speed = update.current_speed_mbps;
        }
        if let Some(_msg) = &update.message {
            // message already logged via handle_progress if needed
        }
    }
}

pub struct App {
    pub state: AppState,
    pub file_tree: FileTree,
    pub upload_queue: UploadQueue,
    pub log_panel: LogPanel,
    pub status_bar: StatusBar,
    pub upload_in_progress: bool,
    pub progress: UploadProgress,
    pub current_cancel_token: Option<CancellationToken>,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            state: AppState::Browser,
            file_tree: FileTree::new(),
            upload_queue: UploadQueue::new(),
            log_panel: LogPanel::new(80),
            status_bar: StatusBar::new(
                "Ready. Tab: switch • j/k scroll logs • a: auto • q: quit".to_string(),
            ),
            upload_in_progress: false,
            progress: UploadProgress::default(),
            current_cancel_token: None,
        };
        app.upload_queue.add("example.nfo".to_string());
        app.log_panel
            .push("UpaPasta v2 started — event-driven TUI ready".to_string());
        app
    }

    pub fn add_to_queue(&mut self, path: String) {
        self.upload_queue.add(path.clone());
        self.status_bar.set("Added to upload queue");
        self.log_panel.push(format!("Queued: {}", path));
    }

    pub fn next_tab(&mut self) {
        self.state = match self.state {
            AppState::Dashboard => AppState::Browser,
            AppState::Browser => AppState::History,
            AppState::History => AppState::Config,
            AppState::Config => AppState::Dashboard,
        };
        self.log_panel.push(format!("Switched to {:?}", self.state));
    }

    pub fn prev_tab(&mut self) {
        self.state = match self.state {
            AppState::Dashboard => AppState::Config,
            AppState::Browser => AppState::Dashboard,
            AppState::History => AppState::Browser,
            AppState::Config => AppState::History,
        };
    }

    pub fn trigger_upload(&mut self) {
        if self.upload_in_progress {
            self.status_bar.set("Upload already running");
            return;
        }
        if self.upload_queue.items.is_empty() {
            self.status_bar
                .set("Queue empty — add files in Browser tab (Enter)");
            return;
        }

        self.upload_in_progress = true;
        self.upload_queue.active = self.upload_queue.items.len();

        let token = CancellationToken::new();
        self.current_cancel_token = Some(token.clone());

        self.progress = UploadProgress {
            start_time: Some(Instant::now()),
            ..Default::default()
        };
        self.status_bar.set(format!(
            "🚀 Upload started ({} files) — streaming real pesto progress (x to cancel)",
            self.upload_queue.items.len()
        ));
        self.log_panel
            .push("=== Starting real pesto::post() (dry-run) ===".to_string());
    }

    pub fn upload_finished(&mut self, success: bool, cancelled: bool) {
        self.upload_in_progress = false;
        self.upload_queue.active = 0;
        self.progress.is_cancelled = cancelled;

        if cancelled {
            self.status_bar.set("Upload cancelled by user");
            self.log_panel.push("=== Upload cancelled ===".to_string());
        } else if success {
            self.status_bar.set("Upload finished successfully");
            self.log_panel.push("=== Upload finished ===".to_string());
        } else {
            self.status_bar.set("Upload finished with errors");
        }
    }

    /// Called from the event loop when we receive a human log line
    pub fn handle_progress(&mut self, msg: String) {
        self.log_panel.push(msg);
    }

    /// Called when we receive a structured progress update (preferred path)
    pub fn handle_progress_update(&mut self, update: ProgressUpdate) {
        // Always log a short version
        if let Some(m) = &update.message {
            self.log_panel.push(m.clone());
        }

        self.progress.apply(&update);

        // Update speed using real elapsed time + bytes
        if let Some(start) = self.progress.start_time {
            let elapsed = start.elapsed().as_secs_f64();
            if elapsed > 0.3 && self.progress.done_bytes > 0 {
                let mb = self.progress.done_bytes as f64 / (1024.0 * 1024.0);
                self.progress.last_speed = mb / elapsed;
            }
        }
    }

    pub fn cancel_upload(&mut self) {
        if !self.upload_in_progress {
            return;
        }
        self.progress.is_cancelled = true;
        if let Some(token) = self.current_cancel_token.take() {
            token.cancel();
        }
        self.status_bar.set("Cancelling upload...");
        self.log_panel
            .push("=== Upload cancellation requested ===".to_string());
    }
}
