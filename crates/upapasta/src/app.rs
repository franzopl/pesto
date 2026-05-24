use crate::events::ProgressUpdate;
use crate::ui::components::{FileTree, LogPanel, StatusBar, UploadQueue};
use pesto::config::{self, Config as PestoConfig, FileConfig, ObfuscateMode};
use std::path::PathBuf;
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

    /// Ring buffer of recent speeds (MB/s) for sparkline
    pub speed_history: Vec<f64>,

    /// Per-file progress (populated when upload starts)
    pub files: Vec<FileProgress>,
}

/// Progress of a single file during an active upload.
#[derive(Debug, Clone)]
pub struct FileProgress {
    pub name: String,
    pub total_segments: u64,
    pub done_segments: u64,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub status: FileStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileStatus {
    #[default]
    Pending,
    Active,
    Done,
    Failed,
}

/// Human-readable summary of the settings that will be used for the next upload.
#[derive(Debug, Clone, Default)]
pub struct UploadSettingsSummary {
    pub obfuscate: String,
    pub compression: String,
    pub par2: String,
    pub groups: String,
    pub from: String,
    pub article_size: String,
    pub verify: String,
}

impl UploadProgress {
    const MAX_HISTORY: usize = 60; // ~1 minute at 1 sample/sec

    pub fn push_speed_sample(&mut self, speed: f64) {
        self.speed_history.push(speed);
        if self.speed_history.len() > Self::MAX_HISTORY {
            self.speed_history.remove(0);
        }
    }
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
            self.push_speed_sample(update.current_speed_mbps);
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
    pub upload_paused: bool,
    pub progress: UploadProgress,
    pub current_cancel_token: Option<CancellationToken>,

    /// Loaded pesto config (if available)
    pub pesto_config: Option<PestoConfig>,
    #[allow(dead_code)]
    pub config_path: Option<PathBuf>,
    #[allow(dead_code)]
    pub config_error: Option<String>,
}

impl App {
    pub fn new() -> Self {
        let (pesto_config, config_path, config_error) = load_pesto_config();

        let status_msg = if let Some(ref cfg) = pesto_config {
            format!(
                "Config loaded ({} server{}) — Ready",
                cfg.all_servers().count(),
                if cfg.all_servers().count() == 1 {
                    ""
                } else {
                    "s"
                }
            )
        } else if let Some(err) = &config_error {
            format!("Config error: {} (using dry-run)", err)
        } else {
            "No config found — using dry-run mode".to_string()
        };

        let mut app = Self {
            state: AppState::Browser,
            file_tree: FileTree::new(),
            upload_queue: UploadQueue::new(),
            log_panel: LogPanel::new(80),
            status_bar: StatusBar::new(status_msg),
            upload_in_progress: false,
            upload_paused: false,
            progress: UploadProgress::default(),
            current_cancel_token: None,
            pesto_config,
            config_path,
            config_error,
        };
        app.upload_queue.add("example.nfo".to_string());
        app.log_panel
            .push("UpaPasta v2 started — event-driven TUI ready".to_string());

        if app.pesto_config.is_some() {
            app.log_panel
                .push("Real NNTP config loaded — real uploads enabled".to_string());
        } else {
            app.log_panel
                .push("Running in dry-run mode (no config)".to_string());
        }

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

        // Initialize per-file tracking
        let file_progress: Vec<FileProgress> = self
            .upload_queue
            .items
            .iter()
            .map(|name| FileProgress {
                name: name.clone(),
                total_segments: 0,
                done_segments: 0,
                total_bytes: 0,
                done_bytes: 0,
                status: FileStatus::Pending,
            })
            .collect();

        self.progress = UploadProgress {
            start_time: Some(Instant::now()),
            speed_history: vec![0.0; 5],
            files: file_progress,
            ..Default::default()
        };

        self.status_bar.set(format!(
            "🚀 Upload started ({} files) — streaming real pesto progress (x to cancel, p to pause)",
            self.upload_queue.items.len()
        ));
        let mode = if self.pesto_config.is_some() {
            "REAL"
        } else {
            "dry-run"
        };
        self.log_panel
            .push(format!("=== Starting pesto::post() [{}] ===", mode));

        // Show effective settings to the user (very important for transparency)
        let settings = self.effective_upload_settings();
        self.log_panel
            .push("--- Effective settings for this upload ---".to_string());
        self.log_panel
            .push(format!("  Obfuscation : {}", settings.obfuscate));
        self.log_panel
            .push(format!("  Compression : {}", settings.compression));
        self.log_panel
            .push(format!("  PAR2        : {}", settings.par2));
        self.log_panel
            .push(format!("  Groups      : {}", settings.groups));
        self.log_panel
            .push("------------------------------------------".to_string());
    }

    pub fn upload_finished(&mut self, success: bool, cancelled: bool) {
        self.upload_in_progress = false;
        self.upload_paused = false;
        self.upload_queue.active = 0;
        self.progress.is_cancelled = cancelled;
        self.progress.files.clear();

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

        // Apply per-file update if present
        if let Some(fu) = &update.file_update {
            if let Some(fp) = self.progress.files.iter_mut().find(|f| f.name == fu.name) {
                fp.done_segments += fu.done_segments;
                fp.done_bytes += fu.done_bytes;
                fp.total_segments = fu.total_segments.max(fp.total_segments);
                fp.total_bytes = fu.total_bytes.max(fp.total_bytes);

                if fu.ok {
                    if fp.done_segments >= fp.total_segments && fp.total_segments > 0 {
                        fp.status = FileStatus::Done;
                    } else {
                        fp.status = FileStatus::Active;
                    }
                } else {
                    fp.status = FileStatus::Failed;
                }
            }
        }

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

    pub fn toggle_pause(&mut self) {
        if !self.upload_in_progress {
            return;
        }

        self.upload_paused = !self.upload_paused;

        if self.upload_paused {
            self.status_bar.set("Upload paused (p to resume)");
            self.log_panel.push("=== Upload paused ===".to_string());
        } else {
            self.status_bar.set("Upload resumed");
            self.log_panel.push("=== Upload resumed ===".to_string());
        }
    }

    /// Returns a user-friendly summary of the settings that will be used
    /// for the next upload (based on loaded config or dry-run defaults).
    pub fn effective_upload_settings(&self) -> UploadSettingsSummary {
        if let Some(cfg) = &self.pesto_config {
            let obfuscate = match cfg.obfuscate {
                ObfuscateMode::None => "None (real filenames)",
                ObfuscateMode::Subject => "Subject only",
                ObfuscateMode::Full => "Full (subject + yEnc name)",
            }
            .to_string();

            let compression = if let Some(fmt) = &cfg.compress_format {
                if cfg.compress_password.is_some() {
                    format!("{} + password", fmt)
                } else {
                    fmt.clone()
                }
            } else {
                "Disabled".to_string()
            };

            let par2 = format!("{}%", cfg.par2);

            let groups = if cfg.groups.is_empty() {
                "Not set".to_string()
            } else {
                cfg.groups.join(", ")
            };

            let from = if cfg.from.contains('@') {
                cfg.from.clone()
            } else {
                "Random identity".to_string()
            };

            let article = format!("{} KB / {} chars", cfg.article_size / 1024, cfg.line_length);

            let verify = if cfg.verify {
                "Enabled (STAT)"
            } else {
                "Disabled"
            }
            .to_string();

            UploadSettingsSummary {
                obfuscate,
                compression,
                par2,
                groups,
                from,
                article_size: article,
                verify,
            }
        } else {
            // Dry-run defaults (what we currently use in build_dry_run_config)
            UploadSettingsSummary {
                obfuscate: "None (dry-run)".to_string(),
                compression: "Disabled (dry-run)".to_string(),
                par2: "5% (dry-run)".to_string(),
                groups: "alt.binaries.test (dry-run)".to_string(),
                from: "upapasta@local (dry-run)".to_string(),
                article_size: "750 KB / 128 chars (dry-run)".to_string(),
                verify: "Disabled (dry-run)".to_string(),
            }
        }
    }
}

/// Try to load pesto configuration from the standard location.
fn load_pesto_config() -> (Option<PestoConfig>, Option<PathBuf>, Option<String>) {
    match config::default_config_path() {
        Some(path) => {
            if path.exists() {
                match FileConfig::load(&path) {
                    Ok(file_cfg) => {
                        let overrides = pesto::config::Overrides::default();
                        match PestoConfig::resolve(file_cfg, overrides) {
                            Ok(cfg) => (Some(cfg), Some(path), None),
                            Err(e) => (None, Some(path), Some(e.to_string())),
                        }
                    }
                    Err(e) => (None, Some(path), Some(e.to_string())),
                }
            } else {
                (None, Some(path), None)
            }
        }
        None => (
            None,
            None,
            Some("Could not determine config path".to_string()),
        ),
    }
}
