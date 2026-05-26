use std::cmp::Reverse;

use crate::catalog::{Catalog, CatalogStats, NewUpload, UploadSummary};
use crate::events::{ProgressUpdate, UploadPhase};
use crate::nzb_viewer::{NzbContents, NzbViewerState};
use crate::prowlarr::{ConnectionStatus, ProwlarrConfig};
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
    NzbVault,
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

    /// Current pipeline phase
    pub phase: UploadPhase,

    /// PAR2 encoding progress (runs concurrently with NNTP posting)
    pub par2_done_slices: usize,
    pub par2_total_slices: usize,

    /// Compression progress (tracked separately for the three-bar display)
    pub compress_total_bytes: u64,
    pub compress_done_bytes: u64,
    pub compress_finished: bool,
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
        if let Some(ref phase) = update.phase {
            // Track compress progress for the three-bar display
            match phase {
                UploadPhase::Compressing {
                    done_bytes,
                    total_bytes,
                } => {
                    if *total_bytes > 0 {
                        self.compress_total_bytes = *total_bytes;
                    }
                    self.compress_done_bytes = *done_bytes;
                }
                _ if self.compress_total_bytes > 0 && !self.compress_finished => {
                    // Phase moved past Compressing → compression is done
                    self.compress_finished = true;
                    self.compress_done_bytes = self.compress_total_bytes;
                }
                _ => {}
            }
            self.phase = phase.clone();
        }
        if let Some((done, total)) = update.par2_slices {
            self.par2_done_slices = done;
            if total > 0 {
                self.par2_total_slices = total;
            }
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

    /// Persistent upload catalog
    pub catalog: Option<Catalog>,

    /// History screen state
    pub history: HistoryState,

    /// Upload start time (to compute duration for the catalog record)
    pub upload_started_at: Option<std::time::Instant>,

    /// Config screen state + per-session overrides
    pub config_state: ConfigState,

    /// When true, draw the upload config panel (replaces NZB detail in browser)
    pub show_upload_confirm: bool,
    /// Selected field index inside the config panel
    pub confirm_field: usize,
    /// True when the selected config-panel field is in text-edit mode
    pub confirm_editing: bool,
    /// Scratch buffer for text fields inside the config panel
    pub confirm_edit_buf: String,
    /// Toggle to reveal the password field value
    pub confirm_show_password: bool,

    /// NZB Vault screen state
    pub vault: VaultState,

    /// Prowlarr integration state
    pub prowlarr: ProwlarrState,
}

/// One entry in the NZB Vault list.
#[derive(Debug, Clone)]
pub struct VaultEntry {
    /// Full path to the `.nzb` file.
    pub path: PathBuf,
    /// Filename (display name).
    pub name: String,
    /// File size in bytes.
    pub file_size: u64,
    /// Last modification time (Unix timestamp).
    pub modified: u64,
    /// Lazily parsed contents (None until the entry is selected).
    pub contents: Option<NzbContents>,
    /// Whether this NZB appears in the catalog.
    pub in_catalog: bool,
}

/// Sort order for the NZB Vault list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VaultSort {
    #[default]
    Date,
    Name,
    Size,
}

/// State for the NZB Vault screen (F4).
#[derive(Debug, Default)]
pub struct VaultState {
    pub entries: Vec<VaultEntry>,
    pub selected: usize,
    pub sort: VaultSort,
    /// NZB viewer overlay (Some when open with `v`)
    pub viewer: Option<NzbViewerState>,
    /// Error message if the vault directory could not be read
    pub load_error: Option<String>,
}

impl VaultState {
    /// Toggle the sort mode cycling through Date → Name → Size → Date.
    pub fn cycle_sort(&mut self) {
        self.sort = match self.sort {
            VaultSort::Date => VaultSort::Name,
            VaultSort::Name => VaultSort::Size,
            VaultSort::Size => VaultSort::Date,
        };
        self.apply_sort();
    }

    pub fn apply_sort(&mut self) {
        match self.sort {
            VaultSort::Date => self.entries.sort_by_key(|e| Reverse(e.modified)),
            VaultSort::Name => self.entries.sort_by(|a, b| a.name.cmp(&b.name)),
            VaultSort::Size => self.entries.sort_by_key(|e| Reverse(e.file_size)),
        }
    }

    pub fn selected_entry(&self) -> Option<&VaultEntry> {
        self.entries.get(self.selected)
    }

    #[allow(dead_code)]
    pub fn selected_entry_mut(&mut self) -> Option<&mut VaultEntry> {
        self.entries.get_mut(self.selected)
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if !self.entries.is_empty() && self.selected < self.entries.len() - 1 {
            self.selected += 1;
        }
    }
}

/// Prowlarr integration state.
#[derive(Debug, Default)]
pub struct ProwlarrState {
    /// Result of the last connection test.
    pub status: ConnectionStatus,
    /// Session overrides for URL and API key (edited in Config screen).
    pub url_override: Option<String>,
    pub api_key_override: Option<String>,
    /// Active search overlay (Some while open).
    pub search: Option<ProwlarrSearchState>,
}

/// State for the Prowlarr search results overlay.
#[derive(Debug)]
pub struct ProwlarrSearchState {
    /// The release name used as the search query.
    pub query: String,
    /// Search is in progress (spinner shown).
    pub searching: bool,
    /// Results returned by Prowlarr.
    pub results: Vec<crate::prowlarr::SearchResult>,
    /// Index of the highlighted result.
    pub selected: usize,
    /// Error from the last search attempt, if any.
    pub error: Option<String>,
    /// A download is in progress for the selected result.
    pub downloading: bool,
}

impl ProwlarrSearchState {
    pub fn new(query: String) -> Self {
        Self {
            query,
            searching: true,
            results: Vec::new(),
            selected: 0,
            error: None,
            downloading: false,
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.results.is_empty() && self.selected < self.results.len() - 1 {
            self.selected += 1;
        }
    }

    pub fn selected_result(&self) -> Option<&crate::prowlarr::SearchResult> {
        self.results.get(self.selected)
    }
}

impl ProwlarrState {
    /// Resolve the effective Prowlarr config from session overrides + pesto config.
    pub fn resolve(&self, pesto_cfg: Option<&pesto::config::Config>) -> Option<ProwlarrConfig> {
        let url = self
            .url_override
            .as_deref()
            .or_else(|| pesto_cfg?.indexer_url.as_deref());
        let key = self
            .api_key_override
            .as_deref()
            .or_else(|| pesto_cfg?.indexer_api_key.as_deref());
        ProwlarrConfig::from_opt(url, key)
    }
}

/// State for the History screen.
#[derive(Debug, Default)]
pub struct HistoryState {
    /// Current search query (empty = show all)
    pub query: String,
    /// Whether the search input is active
    pub searching: bool,
    /// Cached list from last DB query
    pub rows: Vec<UploadSummary>,
    /// Selected row index in the list
    pub selected: usize,
    /// Cached stats
    pub stats: Option<CatalogStats>,
    /// Whether stats panel is expanded
    pub show_stats: bool,
    /// NZB archive viewer overlay (Some when open)
    pub nzb_viewer: Option<NzbViewerState>,
}

/// Per-session upload overrides set via the Config screen.
/// None = use the value from the loaded pesto config (or built-in default).
#[derive(Debug, Default, Clone)]
pub struct SessionOverrides {
    pub from: Option<String>,
    /// Comma-separated newsgroup list.
    pub groups: Option<String>,
    pub obfuscate: Option<ObfuscateMode>,
    /// 0–50 %
    pub par2: Option<u8>,
    pub article_size_kb: Option<usize>,
    pub verify: Option<bool>,
    pub nzb_password: Option<String>,
    pub nzb_category: Option<String>,
    pub compress_password: Option<String>,
}

/// State for the Config screen.
#[derive(Debug, Default)]
pub struct ConfigState {
    /// Index of the selected field in the field list.
    pub selected: usize,
    /// Whether we are currently editing the selected field.
    pub editing: bool,
    /// Scratch buffer for text input.
    pub edit_buf: String,
    /// Per-session overrides the user has set.
    pub overrides: SessionOverrides,
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

        // Open (or create) the catalog and optionally import legacy JSONL
        let catalog = crate::catalog::default_catalog_path().and_then(|p| {
            match Catalog::open(&p) {
                Ok(c) => Some(c),
                Err(e) => {
                    // Catalog failure is non-fatal
                    eprintln!("catalog open error: {e}");
                    None
                }
            }
        });

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
            catalog,
            history: HistoryState::default(),
            upload_started_at: None,
            config_state: ConfigState::default(),
            show_upload_confirm: false,
            confirm_field: 0,
            confirm_editing: false,
            confirm_edit_buf: String::new(),
            confirm_show_password: false,
            vault: VaultState::default(),
            prowlarr: ProwlarrState::default(),
        };
        // Import legacy JSONL once if catalog is empty
        if let Some(ref cat) = app.catalog {
            if !cat.is_populated() {
                if let Some(jsonl) = crate::catalog::legacy_jsonl_path() {
                    if jsonl.exists() {
                        match cat.import_jsonl(&jsonl) {
                            Ok((n, _)) if n > 0 => {
                                app.log_panel
                                    .push(format!("Imported {} records from legacy history", n));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        // Populate history list + browser upload indicators on startup
        app.refresh_history();

        // Do NOT add example files on startup anymore (was confusing users)
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
            AppState::History => AppState::NzbVault,
            AppState::NzbVault => AppState::Config,
            AppState::Config => AppState::Dashboard,
        };
        if self.state == AppState::NzbVault {
            self.load_vault();
        }
        self.log_panel.push(format!("Switched to {:?}", self.state));
    }

    pub fn prev_tab(&mut self) {
        self.state = match self.state {
            AppState::Dashboard => AppState::Config,
            AppState::Browser => AppState::Dashboard,
            AppState::History => AppState::Browser,
            AppState::NzbVault => AppState::History,
            AppState::Config => AppState::NzbVault,
        };
        if self.state == AppState::NzbVault {
            self.load_vault();
        }
    }

    /// Load (or reload) the NZB Vault from the configured nzb_dir.
    pub fn load_vault(&mut self) {
        let nzb_dir = self
            .pesto_config
            .as_ref()
            .and_then(|c| c.nzb_dir.as_deref())
            .map(PathBuf::from);

        let Some(dir) = nzb_dir else {
            self.vault.entries.clear();
            self.vault.load_error = Some("nzb_dir not configured in pesto.toml".to_string());
            return;
        };

        self.vault.load_error = None;

        let read_dir = match std::fs::read_dir(&dir) {
            Ok(d) => d,
            Err(e) => {
                self.vault.entries.clear();
                self.vault.load_error = Some(format!("{}: {}", dir.display(), e));
                return;
            }
        };

        // Collect catalog NZB paths for cross-reference
        let catalog_paths: std::collections::HashSet<String> = if let Some(ref cat) = self.catalog {
            cat.all_nzb_paths()
                .unwrap_or_default()
                .into_iter()
                .collect()
        } else {
            std::collections::HashSet::new()
        };

        let mut entries: Vec<VaultEntry> = read_dir
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|x| x.eq_ignore_ascii_case("nzb"))
                    .unwrap_or(false)
            })
            .map(|e| {
                let path = e.path();
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let meta = e.metadata().ok();
                let file_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let modified = meta
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let in_catalog = catalog_paths.contains(&path.to_string_lossy().to_string());
                VaultEntry {
                    path,
                    name,
                    file_size,
                    modified,
                    contents: None,
                    in_catalog,
                }
            })
            .collect();

        // Apply current sort
        match self.vault.sort {
            VaultSort::Date => entries.sort_by_key(|e| Reverse(e.modified)),
            VaultSort::Name => entries.sort_by(|a, b| a.name.cmp(&b.name)),
            VaultSort::Size => entries.sort_by_key(|e| Reverse(e.file_size)),
        }

        self.vault.selected = 0;
        self.vault.entries = entries;
        let count = self.vault.entries.len();
        self.status_bar.set(format!(
            "NZB Vault — {} file{}",
            count,
            if count == 1 { "" } else { "s" }
        ));
    }

    /// Parse the selected vault entry (lazy, only when needed).
    pub fn vault_parse_selected(&mut self) {
        let idx = self.vault.selected;
        if let Some(entry) = self.vault.entries.get_mut(idx) {
            if entry.contents.is_none() {
                match crate::nzb_viewer::parse_nzb(&entry.path.to_string_lossy()) {
                    Ok(c) => entry.contents = Some(c),
                    Err(e) => {
                        self.status_bar.set(format!("Parse error: {}", e));
                    }
                }
            }
        }
    }

    /// Open the NZB viewer overlay for the selected vault entry.
    pub fn vault_open_viewer(&mut self) {
        self.vault_parse_selected();
        if let Some(entry) = self.vault.selected_entry() {
            if let Some(ref contents) = entry.contents {
                self.vault.viewer = Some(crate::nzb_viewer::NzbViewerState {
                    contents: contents.clone(),
                    scroll: 0,
                });
            }
        }
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
        self.upload_started_at = Some(Instant::now());

        // Mark uploading files in the browser for live [▶] badge
        let uploading_names: std::collections::HashSet<String> = self
            .upload_queue
            .items
            .iter()
            .filter_map(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
            })
            .collect();
        self.file_tree.set_uploading(uploading_names);

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
        let duration_s = self
            .upload_started_at
            .take()
            .map(|t| t.elapsed().as_secs_f64());

        self.upload_in_progress = false;
        self.upload_paused = false;
        self.upload_queue.active = 0;
        self.progress.is_cancelled = cancelled;

        // Record each uploaded file in the catalog
        if success && !cancelled {
            if let Some(ref cat) = self.catalog {
                let size_each = if self.upload_queue.items.is_empty() {
                    None
                } else {
                    self.progress
                        .total_bytes
                        .checked_div(self.upload_queue.items.len() as u64)
                        .map(|b| b as i64)
                };
                let group = self
                    .pesto_config
                    .as_ref()
                    .and_then(|c| c.groups.first().cloned());
                let server = self.pesto_config.as_ref().map(|c| c.host.clone());
                for name in &self.upload_queue.items {
                    let mut rec = NewUpload::from_name(name.clone());
                    rec.size_bytes = size_each;
                    rec.upload_duration_s = duration_s;
                    rec.usenet_group = group.clone();
                    rec.nntp_server = server.clone();
                    let _ = cat.record(&rec);
                }
            }
        }

        self.progress.files.clear();
        self.file_tree
            .set_uploading(std::collections::HashSet::new());

        if cancelled {
            self.status_bar.set("Upload cancelled by user");
            self.log_panel.push("=== Upload cancelled ===".to_string());
        } else if success {
            self.status_bar.set("Upload finished successfully");
            self.log_panel.push("=== Upload finished ===".to_string());
        } else {
            // Keep the UploadError message in the status bar if it was set;
            // only fall back to this generic message if nothing else set it.
            self.log_panel
                .push("=== Upload failed — check logs above ===".to_string());
            if !self.status_bar.message.contains("error") {
                self.status_bar.set("Upload failed — see Dashboard logs");
            }
        }

        // Refresh the history list if it's currently visible
        self.refresh_history();
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
        // Use effective config (with session overrides applied) when available.
        let owned;
        let cfg_ref: Option<&PestoConfig> = if self.pesto_config.is_some() {
            owned = self.effective_config_with_overrides();
            owned.as_ref()
        } else {
            None
        };
        if let Some(cfg) = cfg_ref {
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

    // ── History screen helpers ────────────────────────────────────────────

    /// Reload the history list from the catalog (called on tab switch + after upload).
    pub fn refresh_history(&mut self) {
        let Some(ref cat) = self.catalog else { return };
        let filter = if self.history.query.is_empty() {
            None
        } else {
            Some(self.history.query.as_str())
        };
        match cat.list(filter, 500) {
            Ok(rows) => {
                self.history.rows = rows;
                if self.history.selected >= self.history.rows.len() {
                    self.history.selected = self.history.rows.len().saturating_sub(1);
                }
            }
            Err(e) => {
                self.log_panel.push(format!("catalog list error: {}", e));
            }
        }
        // Refresh the per-file NZB status map used by the file browser.
        match cat.status_map() {
            Ok(map) => self.file_tree.set_nzb_status(map),
            Err(e) => self
                .log_panel
                .push(format!("catalog status_map error: {}", e)),
        }
        if self.history.show_stats {
            self.refresh_stats();
        }
    }

    pub fn refresh_stats(&mut self) {
        let Some(ref cat) = self.catalog else { return };
        match cat.stats() {
            Ok(s) => self.history.stats = Some(s),
            Err(e) => self.log_panel.push(format!("catalog stats error: {}", e)),
        }
    }

    pub fn history_select_next(&mut self) {
        if !self.history.rows.is_empty() {
            self.history.selected = (self.history.selected + 1).min(self.history.rows.len() - 1);
        }
    }

    pub fn history_select_prev(&mut self) {
        self.history.selected = self.history.selected.saturating_sub(1);
    }

    /// Open the NZB viewer for the currently selected history record.
    pub fn open_nzb_viewer(&mut self) {
        let Some(r) = self.history.rows.get(self.history.selected) else {
            return;
        };
        let path = match r.nzb_path.clone() {
            Some(p) => p,
            None => {
                self.status_bar.set("No NZB file for this record");
                return;
            }
        };
        match crate::nzb_viewer::parse_nzb(&path) {
            Ok(contents) => {
                self.history.nzb_viewer = Some(NzbViewerState {
                    contents,
                    scroll: 0,
                });
            }
            Err(e) => {
                self.status_bar.set(format!("NZB parse error: {}", e));
                self.log_panel
                    .push(format!("NZB parse error ({}): {}", path, e));
            }
        }
    }

    pub fn close_nzb_viewer(&mut self) {
        self.history.nzb_viewer = None;
    }

    pub fn nzb_viewer_scroll_down(&mut self) {
        if let Some(ref mut v) = self.history.nzb_viewer {
            let max = v.contents.files.len().saturating_sub(1);
            v.scroll = (v.scroll + 1).min(max);
        }
    }

    pub fn nzb_viewer_scroll_up(&mut self) {
        if let Some(ref mut v) = self.history.nzb_viewer {
            v.scroll = v.scroll.saturating_sub(1);
        }
    }

    // ── Config screen helpers ─────────────────────────────────────────────

    /// Total number of editable fields in the Config screen.
    pub const CONFIG_FIELD_COUNT: usize = 12;

    pub fn config_select_next(&mut self) {
        self.config_state.selected =
            (self.config_state.selected + 1).min(Self::CONFIG_FIELD_COUNT - 1);
    }

    pub fn config_select_prev(&mut self) {
        self.config_state.selected = self.config_state.selected.saturating_sub(1);
    }

    /// Enter edit mode for the currently selected field.
    pub fn config_start_edit(&mut self) {
        let ov = &self.config_state.overrides;
        let cfg = self.pesto_config.as_ref();
        let buf = match self.config_state.selected {
            0 => ov
                .from
                .clone()
                .or_else(|| cfg.map(|c| c.from.clone()))
                .unwrap_or_default(),
            1 => ov
                .groups
                .clone()
                .or_else(|| cfg.map(|c| c.groups.join(",")))
                .unwrap_or_default(),
            2 => {
                // obfuscate: cycle on confirm, no text buf needed
                self.config_cycle_obfuscate();
                return;
            }
            3 => ov
                .par2
                .map(|v| v.to_string())
                .or_else(|| cfg.map(|c| c.par2.to_string()))
                .unwrap_or_else(|| "10".to_string()),
            4 => ov
                .article_size_kb
                .map(|v| v.to_string())
                .or_else(|| cfg.map(|c| (c.article_size / 1024).to_string()))
                .unwrap_or_else(|| "750".to_string()),
            5 => {
                // verify: cycle bool
                self.config_cycle_verify();
                return;
            }
            6 => ov
                .nzb_password
                .clone()
                .or_else(|| cfg.and_then(|c| c.nzb_password.clone()))
                .unwrap_or_default(),
            7 => ov
                .nzb_category
                .clone()
                .or_else(|| cfg.and_then(|c| c.nzb_category.clone()))
                .unwrap_or_default(),
            8 => ov
                .compress_password
                .clone()
                .or_else(|| cfg.and_then(|c| c.compress_password.clone()))
                .unwrap_or_default(),
            // 9 = separator "── Prowlarr ──" (not editable)
            9 => return,
            10 => self
                .prowlarr
                .url_override
                .clone()
                .or_else(|| cfg?.indexer_url.clone())
                .unwrap_or_default(),
            11 => self
                .prowlarr
                .api_key_override
                .clone()
                .or_else(|| cfg?.indexer_api_key.clone())
                .unwrap_or_default(),
            _ => return,
        };
        self.config_state.edit_buf = buf;
        self.config_state.editing = true;
    }

    /// Commit the edit buffer to the current field override.
    pub fn config_confirm_edit(&mut self) {
        let buf = self.config_state.edit_buf.trim().to_string();
        let ov = &mut self.config_state.overrides;
        match self.config_state.selected {
            0 => ov.from = if buf.is_empty() { None } else { Some(buf) },
            1 => ov.groups = if buf.is_empty() { None } else { Some(buf) },
            3 => {
                ov.par2 = buf.parse::<u8>().ok().map(|v| v.min(50));
            }
            4 => {
                ov.article_size_kb = buf.parse::<usize>().ok();
            }
            6 => ov.nzb_password = if buf.is_empty() { None } else { Some(buf) },
            7 => ov.nzb_category = if buf.is_empty() { None } else { Some(buf) },
            8 => ov.compress_password = if buf.is_empty() { None } else { Some(buf) },
            10 => {
                self.prowlarr.url_override = if buf.is_empty() { None } else { Some(buf) };
                // Reset connection status so user can re-test with new URL
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
            }
            11 => {
                self.prowlarr.api_key_override = if buf.is_empty() { None } else { Some(buf) };
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
            }
            _ => {}
        }
        self.config_state.editing = false;
        self.config_state.edit_buf.clear();
        self.status_bar.set("Override saved (session only)");
    }

    pub fn config_cancel_edit(&mut self) {
        self.config_state.editing = false;
        self.config_state.edit_buf.clear();
    }

    /// Reset the selected field override to None (use config default).
    pub fn config_reset_field(&mut self) {
        let ov = &mut self.config_state.overrides;
        match self.config_state.selected {
            0 => ov.from = None,
            1 => ov.groups = None,
            2 => ov.obfuscate = None,
            3 => ov.par2 = None,
            4 => ov.article_size_kb = None,
            5 => ov.verify = None,
            6 => ov.nzb_password = None,
            7 => ov.nzb_category = None,
            8 => ov.compress_password = None,
            10 => {
                self.prowlarr.url_override = None;
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
            }
            11 => {
                self.prowlarr.api_key_override = None;
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
            }
            _ => {}
        }
        self.status_bar.set("Field reset to config default");
    }

    /// Reset all overrides.
    pub fn config_reset_all(&mut self) {
        self.config_state.overrides = SessionOverrides::default();
        self.status_bar.set("All overrides cleared");
    }

    fn config_cycle_obfuscate(&mut self) {
        use ObfuscateMode::*;
        let cfg_default = self
            .pesto_config
            .as_ref()
            .map(|c| c.obfuscate)
            .unwrap_or(ObfuscateMode::None);
        let current = self.config_state.overrides.obfuscate.unwrap_or(cfg_default);
        self.config_state.overrides.obfuscate = Some(match current {
            None => Subject,
            Subject => Full,
            Full => None,
        });
        self.status_bar.set("Obfuscate mode changed");
    }

    fn config_cycle_verify(&mut self) {
        let cfg_default = self
            .pesto_config
            .as_ref()
            .map(|c| c.verify)
            .unwrap_or(false);
        let current = self.config_state.overrides.verify.unwrap_or(cfg_default);
        self.config_state.overrides.verify = Some(!current);
        self.status_bar.set("Verify mode toggled");
    }

    /// Apply session overrides on top of the effective config, returning a
    /// modified clone ready for upload.
    pub fn effective_config_with_overrides(&self) -> Option<PestoConfig> {
        let mut cfg = self.pesto_config.clone()?;
        let ov = &self.config_state.overrides;
        if let Some(ref from) = ov.from {
            cfg.from = from.clone();
        }
        if let Some(ref groups_str) = ov.groups {
            cfg.groups = groups_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(obf) = ov.obfuscate {
            cfg.obfuscate = obf;
        }
        if let Some(par2) = ov.par2 {
            cfg.par2 = par2;
        }
        if let Some(kb) = ov.article_size_kb {
            cfg.article_size = kb * 1024;
        }
        if let Some(verify) = ov.verify {
            cfg.verify = verify;
        }
        if let Some(ref pw) = ov.nzb_password {
            cfg.nzb_password = Some(pw.clone());
        }
        if let Some(ref cat) = ov.nzb_category {
            cfg.nzb_category = Some(cat.clone());
        }
        if let Some(ref pw) = ov.compress_password {
            cfg.compress_password = Some(pw.clone());
        }
        Some(cfg)
    }

    // ── Upload config panel field editing ─────────────────────────────────────

    /// Number of editable fields in the upload config panel.
    /// 0=Obfuscate  1=PAR2%  2=Verify  3=Password  4=Groups
    pub const CONFIRM_FIELDS: usize = 5;

    pub fn confirm_field_next(&mut self) {
        self.confirm_field = (self.confirm_field + 1) % Self::CONFIRM_FIELDS;
    }

    pub fn confirm_field_prev(&mut self) {
        self.confirm_field = if self.confirm_field == 0 {
            Self::CONFIRM_FIELDS - 1
        } else {
            self.confirm_field - 1
        };
    }

    /// Cycle / toggle enum and bool fields. For text fields (3, 4), enter edit mode.
    pub fn confirm_field_activate(&mut self) {
        match self.confirm_field {
            0 => {
                // Obfuscate: cycle None → Subject → Full → None
                let current = self.config_state.overrides.obfuscate.unwrap_or(
                    self.pesto_config
                        .as_ref()
                        .map(|c| c.obfuscate)
                        .unwrap_or(ObfuscateMode::None),
                );
                self.config_state.overrides.obfuscate = Some(match current {
                    ObfuscateMode::None => ObfuscateMode::Subject,
                    ObfuscateMode::Subject => ObfuscateMode::Full,
                    ObfuscateMode::Full => ObfuscateMode::None,
                });
            }
            1 => {
                // PAR2 %: enter text-edit mode
                let current = self
                    .config_state
                    .overrides
                    .par2
                    .unwrap_or(self.pesto_config.as_ref().map(|c| c.par2).unwrap_or(10));
                self.confirm_edit_buf = current.to_string();
                self.confirm_editing = true;
            }
            2 => {
                // Verify: toggle
                let current = self.config_state.overrides.verify.unwrap_or(
                    self.pesto_config
                        .as_ref()
                        .map(|c| c.verify)
                        .unwrap_or(false),
                );
                self.config_state.overrides.verify = Some(!current);
            }
            3 => {
                // NZB Password: enter text-edit mode
                let current = self
                    .config_state
                    .overrides
                    .nzb_password
                    .clone()
                    .or_else(|| {
                        self.pesto_config
                            .as_ref()
                            .and_then(|c| c.nzb_password.clone())
                    })
                    .unwrap_or_default();
                self.confirm_edit_buf = current;
                self.confirm_editing = true;
            }
            4 => {
                // Groups: enter text-edit mode
                let current = self
                    .config_state
                    .overrides
                    .groups
                    .clone()
                    .or_else(|| self.pesto_config.as_ref().map(|c| c.groups.join(", ")))
                    .unwrap_or_default();
                self.confirm_edit_buf = current;
                self.confirm_editing = true;
            }
            _ => {}
        }
    }

    pub fn confirm_field_increment(&mut self) {
        if self.confirm_field == 1 {
            let current = self
                .config_state
                .overrides
                .par2
                .unwrap_or(self.pesto_config.as_ref().map(|c| c.par2).unwrap_or(10));
            self.config_state.overrides.par2 = Some(if current >= 50 { 0 } else { current + 5 });
        }
    }

    pub fn confirm_field_decrement(&mut self) {
        if self.confirm_field == 1 {
            let current = self
                .config_state
                .overrides
                .par2
                .unwrap_or(self.pesto_config.as_ref().map(|c| c.par2).unwrap_or(10));
            self.config_state.overrides.par2 = Some(if current == 0 {
                50
            } else {
                current.saturating_sub(5)
            });
        }
    }

    /// Commit the text edit buffer into the relevant session override.
    pub fn confirm_confirm_edit(&mut self) {
        let buf = self.confirm_edit_buf.trim().to_string();
        match self.confirm_field {
            1 => {
                self.config_state.overrides.par2 = buf.parse::<u8>().ok().map(|v| v.min(50));
            }
            3 => {
                self.config_state.overrides.nzb_password =
                    if buf.is_empty() { None } else { Some(buf) };
            }
            4 => {
                self.config_state.overrides.groups = if buf.is_empty() { None } else { Some(buf) };
            }
            _ => {}
        }
        self.confirm_editing = false;
        self.confirm_edit_buf.clear();
    }

    pub fn confirm_cancel_edit(&mut self) {
        self.confirm_editing = false;
        self.confirm_edit_buf.clear();
    }

    pub fn confirm_toggle_password_reveal(&mut self) {
        self.confirm_show_password = !self.confirm_show_password;
    }

    /// Reset all confirm-panel overrides and close the panel.
    pub fn confirm_close(&mut self) {
        self.show_upload_confirm = false;
        self.confirm_editing = false;
        self.confirm_edit_buf.clear();
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
