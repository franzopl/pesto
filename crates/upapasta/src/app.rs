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
    /// Dedicated upload-queue screen: review, reorder, remove and launch the
    /// queue built in the Browser. The single home for queue management.
    Queue,
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
    /// Whether PAR2 encode + write phases are fully complete.
    pub par2_finished: bool,

    /// Bytes pre-seeded from par2_bytes_hint; consumed as QueueExtended arrives
    /// so total_bytes never jumps backwards.
    pub par2_hint_remaining: u64,

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

// ── Canonical display vocabulary ──────────────────────────────────────────────
//
// One source of truth for how settings are *shown*, so the Dashboard summary,
// the upload-config panel and the Config overrides all read the same. These map
// internal values to display labels only — the stored values (enums, bools and
// the compress-format token used by the cycle handlers) are untouched.

/// Display label for an obfuscation mode: `None` / `Subject` / `Full`.
pub fn obf_label(mode: ObfuscateMode) -> &'static str {
    match mode {
        ObfuscateMode::None => "None",
        ObfuscateMode::Subject => "Subject",
        ObfuscateMode::Full => "Full",
    }
}

/// Display label for an on/off setting.
pub fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "On"
    } else {
        "Off"
    }
}

/// Display label for a compression-format token (`none`/`zip`/`7z`/`rar`).
/// The token stays the logic value used by the cycle handlers; this only
/// controls how it is rendered (`none` → `Off`).
pub fn compress_label(token: &str) -> String {
    match token {
        "none" | "" => "Off".to_string(),
        "zip" => "Zip".to_string(),
        "rar" => "Rar".to_string(),
        other => other.to_string(),
    }
}

/// The marker shown for an unset / empty value, used everywhere.
pub const UNSET: &str = "—";

impl UploadProgress {
    const MAX_HISTORY: usize = 60; // ~1 minute at 1 sample/sec

    pub fn push_speed_sample(&mut self, speed: f64) {
        self.speed_history.push(speed);
        if self.speed_history.len() > Self::MAX_HISTORY {
            self.speed_history.remove(0);
        }
    }

    /// Reset the aggregate gauges for a new queue item. Uploads run one NZB at a
    /// time and each item's progress events restart from zero, while `apply`
    /// only ever grows `done_segments`/`done_bytes` (so a single item's bar
    /// never jumps backwards). Without this reset the previous item's 100% state
    /// swallows the next item's smaller counts and the bar looks frozen. The
    /// per-file rows and the speed-history sparkline are kept; the clock is
    /// restarted so speed/ETA track the current item.
    pub fn reset_for_item(&mut self) {
        self.total_segments = 0;
        self.done_segments = 0;
        self.total_bytes = 0;
        self.done_bytes = 0;
        self.last_speed = 0.0;
        self.start_time = Some(Instant::now());
        self.phase = UploadPhase::default();
        self.par2_done_slices = 0;
        self.par2_total_slices = 0;
        self.par2_finished = false;
        self.par2_hint_remaining = 0;
        self.compress_total_bytes = 0;
        self.compress_done_bytes = 0;
        self.compress_finished = false;
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
        if let Some((seg, bytes)) = update.queue_extended {
            self.total_segments += seg;
            // Absorb real PAR2 bytes against the pre-seeded hint so the bar
            // never goes backwards (same logic as pesto's terminal renderer).
            if bytes <= self.par2_hint_remaining {
                self.par2_hint_remaining -= bytes;
            } else {
                let excess = bytes - self.par2_hint_remaining;
                self.par2_hint_remaining = 0;
                self.total_bytes += excess;
            }
            return;
        }
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
        if update.par2_hint_bytes > 0 {
            self.par2_hint_remaining = update.par2_hint_bytes;
        }
        if update.par2_complete {
            self.par2_finished = true;
            // Ensure slices show as complete even if counts were imprecise.
            if self.par2_total_slices > 0 {
                self.par2_done_slices = self.par2_total_slices;
            }
        }
    }
}

/// Describes how a queued path will become an NZB. A directory bundles every
/// file under it into a single NZB named after the folder (the standard Usenet
/// "release" unit); a plain file becomes one NZB named after the file. This is
/// what makes the queue predictable: one entry → one NZB, never one-per-inner
/// file and never several entries merged together.
#[derive(Debug, Clone)]
pub struct QueueEntryInfo {
    /// Display name of the resulting NZB (without the `.nzb` suffix).
    pub nzb_name: String,
    /// Whether the queued path is a directory (a release bundle).
    pub is_dir: bool,
    /// Number of files the NZB will contain (1 for a plain file).
    pub file_count: usize,
    /// Total bytes the entry will upload (file length, or the sum under a dir).
    pub size_bytes: u64,
    /// Whether `file_count`/`size_bytes` are final. A directory's counts come
    /// from a recursive walk that runs off the UI thread, so a freshly queued
    /// folder starts `sized: false` (counts shown as "…") until the background
    /// job fills them in. Plain files are always `sized: true`.
    pub sized: bool,
}

impl QueueEntryInfo {
    /// File-count label for the UI: the number once known, or "…" while the
    /// background size job is still running for this folder.
    pub fn files_label(&self) -> String {
        if self.sized {
            self.file_count.to_string()
        } else {
            "…".to_string()
        }
    }
}

/// Compute the NZB grouping info for a queued path. For a directory this walks
/// the tree once to count files and sum their sizes so the user can see, before
/// confirming, that a folder becomes a single NZB.
pub fn queue_entry_info(path: &str) -> QueueEntryInfo {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let (file_count, size_bytes) = dir_stats(p);
        let mut info = queue_entry_info_quick(path);
        info.file_count = file_count;
        info.size_bytes = size_bytes;
        info.sized = true;
        info
    } else {
        queue_entry_info_quick(path)
    }
}

/// Like [`queue_entry_info`] but never walks the filesystem: a directory comes
/// back with `sized: false` and zeroed counts, to be filled in later by a
/// background [`dir_stats`] job. Use this on the UI thread (queueing, restoring)
/// so marking a huge folder cannot freeze the loop; a plain file is fully
/// resolved here since it costs a single `stat`.
pub fn queue_entry_info_quick(path: &str) -> QueueEntryInfo {
    let p = std::path::Path::new(path);
    let base = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string();
    if p.is_dir() {
        QueueEntryInfo {
            // A folder name is kept verbatim: dots in a release name are not a
            // file extension and must not be stripped.
            nzb_name: base,
            is_dir: true,
            file_count: 0,
            size_bytes: 0,
            sized: false,
        }
    } else {
        // Strip a single extension for a plain file's NZB stem (movie.mkv → movie).
        let stem = p
            .file_stem()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| base.clone());
        QueueEntryInfo {
            nzb_name: stem,
            is_dir: false,
            file_count: 1,
            size_bytes: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            sized: true,
        }
    }
}

/// Recursively count regular files under `dir` and sum their sizes, stopping at
/// a sane cap so a pathological tree cannot stall the UI. Symlinks are skipped
/// to match `pesto::walk` (which does the same during the real upload).
pub(crate) fn dir_stats(dir: &std::path::Path) -> (usize, u64) {
    const CAP: usize = 100_000;
    let mut stack = vec![dir.to_path_buf()];
    let mut count = 0usize;
    let mut bytes = 0u64;
    while let Some(d) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            } else if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                count += 1;
                bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                if count >= CAP {
                    return (count, bytes);
                }
            }
        }
    }
    (count, bytes)
}

pub struct App {
    pub state: AppState,
    pub file_tree: FileTree,
    pub upload_queue: UploadQueue,
    /// Cached NZB grouping info per queued path, keyed by the absolute path
    /// string stored in `upload_queue.items`. Kept in sync on every queue
    /// mutation so the UI never re-walks directories on the render hot path.
    pub queue_meta: std::collections::HashMap<String, QueueEntryInfo>,
    /// Queued folder paths whose file count / size still need the recursive
    /// `dir_stats` walk. The run loop drains this, runs the walk off the UI
    /// thread, and folds the result back via [`apply_queue_meta`], so marking a
    /// huge folder never blocks the loop.
    pub pending_meta: Vec<String>,
    /// Live per-item upload state, keyed by the queue path. Drives the ✓/✗/▶
    /// icons in the queue view and survives a partial batch so a failed item
    /// can be retried without losing the record of the ones that succeeded.
    pub queue_status: std::collections::HashMap<String, FileStatus>,
    /// Incremented on every Tick event — drives spinner animations in the UI.
    pub tick_count: u64,
    pub log_panel: LogPanel,
    pub status_bar: StatusBar,
    pub upload_in_progress: bool,
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
    /// Where this NZB came from.
    pub origin: NzbOrigin,
}

/// Where a vault NZB file originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NzbOrigin {
    /// Created by an upapasta upload (`nzb_dir/uploaded/`).
    Uploaded,
    /// Downloaded from a Prowlarr/indexer search (`nzb_dir/downloaded/`).
    Downloaded,
    /// Added manually by the user (root of `nzb_dir`).
    #[default]
    Manual,
}

/// Lightweight metadata about an `.nzb` found on disk, keyed by release key in
/// the browser's disk index. Lets the Browser distinguish a Prowlarr download
/// from a prior upload and flag password-protected releases without consulting
/// the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskNzbInfo {
    /// Origin derived from the immediate parent directory name.
    pub origin: NzbOrigin,
    /// True when the NZB head carries a `<meta type="password">` tag.
    pub has_password: bool,
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
    /// Progress of an in-flight "search the whole queue" batch (Some while running).
    pub batch: Option<ProwlarrBatchState>,
}

/// Progress of a batch search over every queued item.
///
/// Drives the queue auto-fetch: each queued release is searched on Prowlarr and
/// an exact-name match is downloaded directly. Non-exact matches are only
/// counted (and logged) — never auto-downloaded.
#[derive(Debug, Default, Clone)]
pub struct ProwlarrBatchState {
    /// Number of queued items processed so far.
    pub done: usize,
    /// Total queued items to process.
    pub total: usize,
    /// Releases auto-downloaded (exact name match found).
    pub downloaded: usize,
    /// Releases searched but with no exact-name match.
    pub no_match: usize,
    /// Search or download errors.
    pub failed: usize,
    /// Release name currently being searched.
    pub current: String,
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
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
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
    /// Compression archive format: `none`, `zip`, `7z` or `rar`.
    pub compress_format: Option<String>,
    /// How a queued directory becomes NZB(s). `None` = the default (`single`).
    pub folder_mode: Option<FolderMode>,
}

/// How a queued directory is turned into NZB(s) at upload time.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FolderMode {
    /// One NZB for the whole folder (a single release). The default.
    #[default]
    Single,
    /// One NZB per file inside the folder (no combined release NZB).
    PerFile,
    /// One NZB per file *and* a combined "season" NZB over all of them.
    Season,
}

impl FolderMode {
    pub fn label(self) -> &'static str {
        match self {
            FolderMode::Single => "single NZB",
            FolderMode::PerFile => "per-file",
            FolderMode::Season => "season (per-file + combined)",
        }
    }

    pub fn next(self) -> Self {
        match self {
            FolderMode::Single => FolderMode::PerFile,
            FolderMode::PerFile => FolderMode::Season,
            FolderMode::Season => FolderMode::Single,
        }
    }
}

/// One editable setting in the upload-config panel. The order here is the order
/// shown on screen and navigated with j/k; both the render and the key handlers
/// derive from this single list, so there are no fragile parallel indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmField {
    Obfuscate,
    Par2,
    FolderMode,
    Compress,
    CompressPassword,
    Verify,
    NzbPassword,
    Groups,
    From,
    Category,
    ArticleSize,
}

/// A rendered snapshot of one field for the panel.
pub struct ConfirmFieldView {
    pub label: &'static str,
    pub value: String,
    pub hint: &'static str,
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
            queue_meta: std::collections::HashMap::new(),
            pending_meta: Vec::new(),
            queue_status: std::collections::HashMap::new(),
            tick_count: 0,
            log_panel: LogPanel::new(80),
            status_bar: StatusBar::new(status_msg),
            upload_in_progress: false,
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
        // Index existing .nzb files in nzb_dir so the browser flags releases
        // that already have an NZB even when the catalog has no record.
        app.refresh_nzb_disk_index();

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

    /// Toggle the item under the Browser cursor in the upload queue, then
    /// advance the cursor. This is the single selection action (`Space`): the
    /// queue is the one source of truth, so the Browser `[x]` badge and the
    /// queue panel always agree. Files and directories are both allowed; a
    /// directory is queued as one release → one NZB.
    pub fn toggle_queue_at_cursor(&mut self) {
        let path = match self.file_tree.get_selected().cloned() {
            Some(p) => p,
            None => return,
        };
        let key = path.to_string_lossy().to_string();
        let now_queued = self.upload_queue.toggle(key.clone());
        if now_queued {
            // Quick (no walk): a folder's file count / size is computed off the
            // UI thread so marking a huge directory never freezes the loop.
            let info = queue_entry_info_quick(&key);
            if info.is_dir {
                self.pending_meta.push(key.clone());
                self.status_bar.set(format!(
                    "Queued folder “{}” → 1 NZB (sizing…) — {} in queue",
                    info.nzb_name,
                    self.upload_queue.items.len()
                ));
            } else {
                self.status_bar.set(format!(
                    "Queued “{}” — {} in queue",
                    info.nzb_name,
                    self.upload_queue.items.len()
                ));
            }
            self.queue_meta.insert(key, info);
        } else {
            self.queue_meta.remove(&key);
            self.status_bar.set(format!(
                "Unqueued — {} item(s) in queue",
                self.upload_queue.items.len()
            ));
        }
        self.sync_queue_badges();
        self.save_queue();
        self.file_tree.select_next();
    }

    /// Rebuild the Browser badge mirror from the queue. Must be called after any
    /// mutation of `upload_queue.items`.
    pub fn sync_queue_badges(&mut self) {
        let set: std::collections::HashSet<PathBuf> =
            self.upload_queue.items.iter().map(PathBuf::from).collect();
        self.file_tree.set_queued(set);
    }

    /// Grouping info for a queued path, from the cache when available. The
    /// fallback uses the quick (walk-free) form so a render that races ahead of
    /// the cache cannot trigger a filesystem walk on the UI thread.
    pub fn queue_info(&self, path: &str) -> QueueEntryInfo {
        self.queue_meta
            .get(path)
            .cloned()
            .unwrap_or_else(|| queue_entry_info_quick(path))
    }

    /// Drain the folders awaiting a `dir_stats` walk. The run loop runs these on
    /// a blocking worker and returns each result via [`apply_queue_meta`].
    pub fn take_pending_meta(&mut self) -> Vec<String> {
        std::mem::take(&mut self.pending_meta)
    }

    /// Fold a completed `dir_stats` result back into the queue cache. Ignored if
    /// the path has since left the queue (unqueued before the walk finished).
    pub fn apply_queue_meta(&mut self, key: &str, file_count: usize, size_bytes: u64) {
        if let Some(info) = self.queue_meta.get_mut(key) {
            info.file_count = file_count;
            info.size_bytes = size_bytes;
            info.sized = true;
        }
    }

    /// Remove the selected queue item, keeping caches and badges in sync.
    pub fn remove_queue_selected(&mut self) -> Option<String> {
        let removed = self.upload_queue.remove_selected();
        if let Some(ref p) = removed {
            self.queue_meta.remove(p);
            self.sync_queue_badges();
            self.save_queue();
        }
        removed
    }

    /// Clear the whole queue, returning how many items were removed.
    pub fn clear_queue(&mut self) -> usize {
        let count = self.upload_queue.items.len();
        self.upload_queue.clear();
        self.queue_meta.clear();
        self.sync_queue_badges();
        self.save_queue();
        count
    }

    pub fn next_tab(&mut self) {
        self.state = match self.state {
            AppState::Dashboard => AppState::Queue,
            AppState::Queue => AppState::Browser,
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
            AppState::Queue => AppState::Dashboard,
            AppState::Browser => AppState::Queue,
            AppState::History => AppState::Browser,
            AppState::NzbVault => AppState::History,
            AppState::Config => AppState::NzbVault,
        };
        if self.state == AppState::NzbVault {
            self.load_vault();
        }
    }

    /// Load (or reload) the NZB Vault from the configured nzb_dir.
    ///
    /// Recursively scans all subdirectories. Origin is determined by the
    /// immediate parent folder name: `uploaded/` → Uploaded, `downloaded/` →
    /// Downloaded, anything else (including the root) → Manual.
    pub fn load_vault(&mut self) {
        let nzb_dir = self
            .pesto_config
            .as_ref()
            .and_then(|c| c.nzb_dir.as_deref())
            .map(expand_tilde);

        let Some(dir) = nzb_dir else {
            self.vault.entries.clear();
            self.vault.load_error = Some("nzb_dir not configured in pesto.toml".to_string());
            return;
        };

        if !dir.is_dir() {
            self.vault.entries.clear();
            self.vault.load_error = Some(format!("{}: directory not found", dir.display()));
            return;
        }

        self.vault.load_error = None;

        // Collect catalog NZB paths for cross-reference
        let catalog_paths: std::collections::HashSet<String> = if let Some(ref cat) = self.catalog {
            cat.all_nzb_paths()
                .unwrap_or_default()
                .into_iter()
                .collect()
        } else {
            std::collections::HashSet::new()
        };

        let mut entries: Vec<VaultEntry> = Vec::new();
        collect_nzbs_recursive(&dir, &catalog_paths, &mut entries);

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

        // Reset per-item state to Pending. The live [▶] badge and Active state
        // are then driven one item at a time by ItemUploadStarted, because
        // uploads run sequentially (one NZB at a time).
        self.queue_status = self
            .upload_queue
            .items
            .iter()
            .map(|p| (p.clone(), FileStatus::Pending))
            .collect();
        self.file_tree
            .set_uploading(std::collections::HashSet::new());

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
            "🚀 Upload started ({} files) — streaming real pesto progress (x to cancel)",
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

    /// Live upload state for a queued path (Pending when unknown).
    pub fn item_status(&self, path: &str) -> FileStatus {
        self.queue_status
            .get(path)
            .copied()
            .unwrap_or(FileStatus::Pending)
    }

    /// A single queue item began uploading. Mark it Active and light up its
    /// live [▶] badge in the Browser (only this item, since uploads are
    /// sequential).
    pub fn item_upload_started(&mut self, path: &str) {
        // Each item posts its own NZB and restarts its progress from zero, so
        // clear the previous item's gauges; otherwise the bar stays pinned at
        // the last item's 100% (apply() only grows the counters).
        self.progress.reset_for_item();
        self.queue_status
            .insert(path.to_string(), FileStatus::Active);
        if let Some(fp) = self.progress.files.iter_mut().find(|f| f.name == path) {
            fp.status = FileStatus::Active;
        }
        let basename = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        let mut set = std::collections::HashSet::new();
        if let Some(b) = basename {
            set.insert(b);
        }
        self.file_tree.set_uploading(set);

        // Drop the previous item's rows. Folder modes (PerFile/Season) post each
        // inner file under its own `real_name`, so the per-file panel is seeded
        // from this item's pesto `Started` event (see `register_upload_files`),
        // not from the folder-keyed queue entry.
        self.progress.files.clear();
    }

    /// Seed the per-file rows from a pesto run's work plan. Each tuple is
    /// `(real_name, total_segments, total_bytes)` taken from the `Started`
    /// event; the same `real_name` keys the later `SegmentDone` updates, so the
    /// per-file gauges advance instead of sitting at "waiting…". Within one
    /// queue entry several runs can register files (Season posts each episode as
    /// its own run), so rows accumulate across runs and are matched by name.
    pub fn register_upload_files(&mut self, files: Vec<(String, u64, u64)>) {
        for (name, total_segments, total_bytes) in files {
            if let Some(fp) = self.progress.files.iter_mut().find(|f| f.name == name) {
                fp.total_segments = total_segments.max(fp.total_segments);
                fp.total_bytes = total_bytes.max(fp.total_bytes);
                if fp.status == FileStatus::Pending {
                    fp.status = FileStatus::Active;
                }
            } else {
                self.progress.files.push(FileProgress {
                    name,
                    total_segments,
                    done_segments: 0,
                    total_bytes,
                    done_bytes: 0,
                    status: FileStatus::Active,
                });
            }
        }
    }

    /// A single queue item finished. Record it in the catalog immediately with
    /// the real size and the real NZB path pesto wrote, so a later failure in
    /// the same batch can never erase this success. Failed items are kept in
    /// the queue (marked ✗) for retry.
    pub fn item_upload_done(
        &mut self,
        path: &str,
        success: bool,
        size_bytes: u64,
        nzb_path: Option<PathBuf>,
        duration_s: f64,
        record_catalog: bool,
    ) {
        let status = if success {
            FileStatus::Done
        } else {
            FileStatus::Failed
        };
        self.queue_status.insert(path.to_string(), status);
        if let Some(fp) = self.progress.files.iter_mut().find(|f| f.name == path) {
            fp.status = status;
            if success && fp.total_segments == 0 {
                // No per-file segment stream matched; show a full gauge anyway.
                fp.total_segments = 1;
                fp.done_segments = 1;
            }
        }

        // Per-file / season folder modes record each produced NZB separately via
        // `CatalogRecord`, so the item-level event must not double-record.
        if success && record_catalog {
            let original_name = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_string();
            self.record_catalog_entry(original_name, size_bytes, nzb_path, duration_s);
        }
    }

    /// Record one produced NZB in the catalog and refresh the Browser status.
    pub fn record_catalog_entry(
        &mut self,
        original_name: String,
        size_bytes: u64,
        nzb_path: Option<PathBuf>,
        duration_s: f64,
    ) {
        if let Some(ref cat) = self.catalog {
            let group = self
                .pesto_config
                .as_ref()
                .and_then(|c| c.groups.first().cloned());
            let server = self.pesto_config.as_ref().map(|c| c.host.clone());

            let mut rec = NewUpload::from_name(original_name);
            rec.size_bytes = (size_bytes > 0).then_some(size_bytes as i64);
            rec.upload_duration_s = Some(duration_s);
            rec.usenet_group = group;
            rec.nntp_server = server;
            rec.nzb_path = nzb_path.map(|p| p.to_string_lossy().into_owned());
            if let Err(e) = cat.record(&rec) {
                self.log_panel.push(format!("catalog record error: {}", e));
            }
        }
        // Reflect the new catalog entry in the Browser's NZB status column.
        self.refresh_nzb_status();
    }

    /// Refresh only the per-file NZB status map used by the Browser (cheaper
    /// than a full history refresh; safe to call after each item).
    pub fn refresh_nzb_status(&mut self) {
        if let Some(ref cat) = self.catalog {
            if let Ok(map) = cat.status_map() {
                self.file_tree.set_nzb_status(map);
            }
        }
    }

    /// Build the on-disk NZB index by scanning the configured `nzb_dir`
    /// recursively for `.nzb` files and keying them by release name. This is
    /// what lets the Browser flag a file as already-uploaded when a matching
    /// NZB exists on disk but is not in the catalog (e.g. uploaded by another
    /// tool, or before this catalog existed). No-op when `nzb_dir` is unset.
    pub fn refresh_nzb_disk_index(&mut self) {
        let Some(dir) = self
            .pesto_config
            .as_ref()
            .and_then(|c| c.nzb_dir.as_deref())
            .map(expand_tilde)
        else {
            return;
        };
        let mut index = std::collections::HashMap::new();
        collect_nzb_release_keys(&dir, &mut index);
        self.file_tree.set_nzb_disk_index(index);
    }

    pub fn upload_finished(&mut self, success: bool, cancelled: bool) {
        // Catalog records are written per-item in `item_upload_done`, with the
        // real size and NZB path; nothing is recorded here. This finalizes the
        // batch UI and prunes the queue.
        self.upload_started_at.take();

        self.upload_in_progress = false;
        self.upload_queue.active = 0;
        self.progress.is_cancelled = cancelled;

        // On a non-cancelled batch, successfully uploaded items leave the queue
        // (they now live in History with a ✓ badge); failed items stay queued
        // so the user can fix the problem and press `u` to retry just them.
        let mut failed = 0usize;
        if !cancelled {
            let done: Vec<String> = self
                .upload_queue
                .items
                .iter()
                .filter(|p| self.item_status(p) == FileStatus::Done)
                .cloned()
                .collect();
            failed = self
                .upload_queue
                .items
                .iter()
                .filter(|p| self.item_status(p) == FileStatus::Failed)
                .count();
            for p in &done {
                if let Some(pos) = self.upload_queue.items.iter().position(|q| q == p) {
                    self.upload_queue.items.remove(pos);
                }
                self.queue_meta.remove(p);
                self.queue_status.remove(p);
            }
            let len = self.upload_queue.items.len();
            if self.upload_queue.selected >= len {
                self.upload_queue.selected = len.saturating_sub(1);
            }
            self.sync_queue_badges();
            self.save_queue();
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
            self.log_panel
                .push("=== Upload finished with failures — check logs above ===".to_string());
            self.status_bar.set(format!(
                "{} item(s) failed — still queued, press u to retry",
                failed
            ));
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

        // Apply per-file update if present. The row is normally seeded from the
        // run's `Started` event (register_upload_files); fall back to creating it
        // here so a SegmentDone is never silently dropped.
        if let Some(fu) = &update.file_update {
            let fp = match self.progress.files.iter_mut().find(|f| f.name == fu.name) {
                Some(fp) => fp,
                None => {
                    self.progress.files.push(FileProgress {
                        name: fu.name.clone(),
                        total_segments: 0,
                        done_segments: 0,
                        total_bytes: 0,
                        done_bytes: 0,
                        status: FileStatus::Active,
                    });
                    self.progress.files.last_mut().unwrap()
                }
            };
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
            let obfuscate = obf_label(cfg.obfuscate).to_string();

            let compression = match &cfg.compress_format {
                Some(fmt) if cfg.compress_password.is_some() => {
                    format!("{} + password", compress_label(fmt))
                }
                Some(fmt) => compress_label(fmt),
                None => "Off".to_string(),
            };

            let par2 = format!("{}%", cfg.par2);

            let groups = if cfg.groups.is_empty() {
                UNSET.to_string()
            } else {
                cfg.groups.join(", ")
            };

            let from = if cfg.from.contains('@') {
                cfg.from.clone()
            } else {
                "Random identity".to_string()
            };

            let article = format!("{} KB / {} chars", cfg.article_size / 1024, cfg.line_length);

            let verify = on_off(cfg.verify).to_string();

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
                compression: "Off (dry-run)".to_string(),
                par2: "5% (dry-run)".to_string(),
                groups: "alt.binaries.test (dry-run)".to_string(),
                from: "upapasta@local (dry-run)".to_string(),
                article_size: "750 KB / 128 chars (dry-run)".to_string(),
                verify: "Off (dry-run)".to_string(),
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
                let val = if buf.is_empty() { None } else { Some(buf) };
                self.prowlarr.url_override = val.clone();
                // Reset connection status so user can re-test with new URL
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
                // Prowlarr config is persisted to config.toml so it survives
                // restarts (unlike the upload overrides, which are session-only).
                self.persist_indexer_field("url", val.as_deref());
                self.config_state.editing = false;
                self.config_state.edit_buf.clear();
                return;
            }
            11 => {
                let val = if buf.is_empty() { None } else { Some(buf) };
                self.prowlarr.api_key_override = val.clone();
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
                self.persist_indexer_field("api_key", val.as_deref());
                self.config_state.editing = false;
                self.config_state.edit_buf.clear();
                return;
            }
            _ => {}
        }
        self.config_state.editing = false;
        self.config_state.edit_buf.clear();
        self.status_bar.set("Override saved (session only)");
    }

    /// Persist a `[output.indexer]` field (e.g. `url`, `api_key`) to the pesto
    /// `config.toml` so Prowlarr settings survive a restart. Uses `toml_edit` to
    /// preserve the rest of the file (comments, formatting, ordering). A `None`
    /// value removes the key. Also updates the in-memory resolved config so the
    /// change takes effect immediately even after the session override is reset.
    fn persist_indexer_field(&mut self, field: &str, value: Option<&str>) {
        // Mirror the change into the already-resolved in-memory config.
        if let Some(cfg) = self.pesto_config.as_mut() {
            let owned = value.map(str::to_string);
            match field {
                "url" => cfg.indexer_url = owned,
                "api_key" => cfg.indexer_api_key = owned,
                _ => {}
            }
        }

        let Some(path) = self
            .config_path
            .clone()
            .or_else(pesto::config::default_config_path)
        else {
            self.status_bar
                .set("Saved for this session (could not locate config.toml)");
            return;
        };

        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let new_text = match apply_indexer_field(&text, field, value) {
            Ok(t) => t,
            Err(e) => {
                self.status_bar.set(format!(
                    "Saved for this session (config.toml parse error: {e})"
                ));
                return;
            }
        };

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, new_text) {
            Ok(()) => self
                .status_bar
                .set(format!("Saved Prowlarr {field} to {}", path.display())),
            Err(e) => self
                .status_bar
                .set(format!("Saved for this session (write failed: {e})")),
        }
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
        if let Some(ref fmt) = ov.compress_format {
            cfg.compress_format = if fmt == "none" {
                None
            } else {
                Some(fmt.clone())
            };
        }
        Some(cfg)
    }

    /// The effective folder mode for this batch (override or the default).
    pub fn effective_folder_mode(&self) -> FolderMode {
        self.config_state.overrides.folder_mode.unwrap_or_default()
    }

    // ── Upload config panel field editing ─────────────────────────────────────

    /// The fields shown in the panel, in order. `Folder mode` only appears when
    /// a directory is queued (it is a no-op for plain files).
    pub fn confirm_order(&self) -> Vec<ConfirmField> {
        use ConfirmField::*;
        let has_dir = self
            .upload_queue
            .items
            .iter()
            .any(|p| self.queue_info(p).is_dir);
        let mut v = vec![Obfuscate, Par2];
        if has_dir {
            v.push(FolderMode);
        }
        v.extend([
            Compress,
            CompressPassword,
            Verify,
            NzbPassword,
            Groups,
            From,
            Category,
            ArticleSize,
        ]);
        v
    }

    /// The field currently under the cursor.
    fn current_confirm_field(&self) -> Option<ConfirmField> {
        self.confirm_order().get(self.confirm_field).copied()
    }

    pub fn confirm_field_next(&mut self) {
        let len = self.confirm_order().len().max(1);
        self.confirm_field = (self.confirm_field + 1) % len;
    }

    pub fn confirm_field_prev(&mut self) {
        let len = self.confirm_order().len().max(1);
        self.confirm_field = if self.confirm_field == 0 {
            len - 1
        } else {
            self.confirm_field - 1
        };
    }

    fn obf_effective(&self) -> ObfuscateMode {
        self.config_state.overrides.obfuscate.unwrap_or(
            self.pesto_config
                .as_ref()
                .map(|c| c.obfuscate)
                .unwrap_or(ObfuscateMode::None),
        )
    }

    fn par2_effective(&self) -> u8 {
        self.config_state
            .overrides
            .par2
            .unwrap_or(self.pesto_config.as_ref().map(|c| c.par2).unwrap_or(10))
    }

    fn compress_effective(&self) -> String {
        self.config_state
            .overrides
            .compress_format
            .clone()
            .or_else(|| {
                self.pesto_config
                    .as_ref()
                    .and_then(|c| c.compress_format.clone())
            })
            .unwrap_or_else(|| "none".to_string())
    }

    /// Cycle / toggle enum and bool fields; for text/number fields, enter edit mode.
    pub fn confirm_field_activate(&mut self) {
        let Some(field) = self.current_confirm_field() else {
            return;
        };
        match field {
            ConfirmField::Obfuscate => self.confirm_cycle_obfuscate(true),
            ConfirmField::FolderMode => {
                let cur = self.effective_folder_mode();
                self.config_state.overrides.folder_mode = Some(cur.next());
            }
            ConfirmField::Compress => self.confirm_cycle_compress(true),
            ConfirmField::Verify => self.confirm_toggle_verify(),
            // Number / text fields → enter edit mode prefilled with the current value.
            ConfirmField::Par2 => self.confirm_start_edit(self.par2_effective().to_string()),
            ConfirmField::ArticleSize => {
                let kb = self.config_state.overrides.article_size_kb.unwrap_or(
                    self.pesto_config
                        .as_ref()
                        .map(|c| c.article_size / 1024)
                        .unwrap_or(768),
                );
                self.confirm_start_edit(kb.to_string());
            }
            ConfirmField::CompressPassword => {
                let cur = self
                    .config_state
                    .overrides
                    .compress_password
                    .clone()
                    .or_else(|| {
                        self.pesto_config
                            .as_ref()
                            .and_then(|c| c.compress_password.clone())
                    })
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
            ConfirmField::NzbPassword => {
                let cur = self
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
                self.confirm_start_edit(cur);
            }
            ConfirmField::Groups => {
                let cur = self
                    .config_state
                    .overrides
                    .groups
                    .clone()
                    .or_else(|| self.pesto_config.as_ref().map(|c| c.groups.join(", ")))
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
            ConfirmField::From => {
                let cur = self
                    .config_state
                    .overrides
                    .from
                    .clone()
                    .or_else(|| self.pesto_config.as_ref().map(|c| c.from.clone()))
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
            ConfirmField::Category => {
                let cur = self
                    .config_state
                    .overrides
                    .nzb_category
                    .clone()
                    .or_else(|| {
                        self.pesto_config
                            .as_ref()
                            .and_then(|c| c.nzb_category.clone())
                    })
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
        }
    }

    fn confirm_start_edit(&mut self, prefill: String) {
        self.confirm_edit_buf = prefill;
        self.confirm_editing = true;
    }

    fn confirm_cycle_obfuscate(&mut self, _forward: bool) {
        let next = match self.obf_effective() {
            ObfuscateMode::None => ObfuscateMode::Subject,
            ObfuscateMode::Subject => ObfuscateMode::Full,
            ObfuscateMode::Full => ObfuscateMode::None,
        };
        self.config_state.overrides.obfuscate = Some(next);
    }

    fn confirm_cycle_compress(&mut self, _forward: bool) {
        let next = match self.compress_effective().as_str() {
            "none" => "zip",
            "zip" => "7z",
            "7z" => "rar",
            _ => "none",
        };
        self.config_state.overrides.compress_format = Some(next.to_string());
    }

    fn confirm_toggle_verify(&mut self) {
        let cur = self.config_state.overrides.verify.unwrap_or(
            self.pesto_config
                .as_ref()
                .map(|c| c.verify)
                .unwrap_or(false),
        );
        self.config_state.overrides.verify = Some(!cur);
    }

    /// `→` / `l` / Space: advance cycle/number/toggle fields in place.
    pub fn confirm_field_increment(&mut self) {
        match self.current_confirm_field() {
            Some(ConfirmField::Obfuscate) => self.confirm_cycle_obfuscate(true),
            Some(ConfirmField::Compress) => self.confirm_cycle_compress(true),
            Some(ConfirmField::Verify) => self.confirm_toggle_verify(),
            Some(ConfirmField::FolderMode) => {
                let cur = self.effective_folder_mode();
                self.config_state.overrides.folder_mode = Some(cur.next());
            }
            Some(ConfirmField::Par2) => {
                let cur = self.par2_effective();
                self.config_state.overrides.par2 = Some(if cur >= 50 { 0 } else { cur + 5 });
            }
            _ => {}
        }
    }

    /// `←` / `h`: step cycle/number/toggle fields backwards.
    pub fn confirm_field_decrement(&mut self) {
        match self.current_confirm_field() {
            // Cycles are short; stepping backwards is the same as cycling forward.
            Some(ConfirmField::Obfuscate) => self.confirm_cycle_obfuscate(false),
            Some(ConfirmField::Compress) => self.confirm_cycle_compress(false),
            Some(ConfirmField::Verify) => self.confirm_toggle_verify(),
            Some(ConfirmField::FolderMode) => {
                let cur = self.effective_folder_mode();
                self.config_state.overrides.folder_mode = Some(cur.next());
            }
            Some(ConfirmField::Par2) => {
                let cur = self.par2_effective();
                self.config_state.overrides.par2 =
                    Some(if cur == 0 { 50 } else { cur.saturating_sub(5) });
            }
            _ => {}
        }
    }

    /// Commit the text edit buffer into the relevant session override.
    pub fn confirm_confirm_edit(&mut self) {
        let buf = self.confirm_edit_buf.trim().to_string();
        let set_opt = |b: String| if b.is_empty() { None } else { Some(b) };
        match self.current_confirm_field() {
            Some(ConfirmField::Par2) => {
                self.config_state.overrides.par2 = buf.parse::<u8>().ok().map(|v| v.min(50));
            }
            Some(ConfirmField::ArticleSize) => {
                self.config_state.overrides.article_size_kb =
                    buf.parse::<usize>().ok().filter(|kb| *kb > 0);
            }
            Some(ConfirmField::CompressPassword) => {
                self.config_state.overrides.compress_password = set_opt(buf);
            }
            Some(ConfirmField::NzbPassword) => {
                self.config_state.overrides.nzb_password = set_opt(buf);
            }
            Some(ConfirmField::Groups) => {
                self.config_state.overrides.groups = set_opt(buf);
            }
            Some(ConfirmField::From) => {
                self.config_state.overrides.from = set_opt(buf);
            }
            Some(ConfirmField::Category) => {
                self.config_state.overrides.nzb_category = set_opt(buf);
            }
            _ => {}
        }
        self.confirm_editing = false;
        self.confirm_edit_buf.clear();
    }

    /// Rendered snapshot of all panel fields, in display order.
    pub fn confirm_field_views(&self) -> Vec<ConfirmFieldView> {
        let ov = &self.config_state.overrides;
        let cfg = self.pesto_config.as_ref();
        let mask = |raw: &str| -> String {
            if raw.is_empty() {
                UNSET.to_string()
            } else if self.confirm_show_password {
                raw.to_string()
            } else {
                "•".repeat(raw.len().min(20))
            }
        };
        self.confirm_order()
            .into_iter()
            .map(|field| {
                let (label, value, hint): (&'static str, String, &'static str) = match field {
                    ConfirmField::Obfuscate => (
                        "Obfuscate",
                        obf_label(self.obf_effective()).to_string(),
                        "←→ cycle",
                    ),
                    ConfirmField::Par2 => (
                        "PAR2 %",
                        format!("{}%", self.par2_effective()),
                        "←→ or Enter",
                    ),
                    ConfirmField::FolderMode => (
                        "Folder",
                        self.effective_folder_mode().label().to_string(),
                        "←→ cycle",
                    ),
                    ConfirmField::Compress => (
                        "Compress",
                        compress_label(&self.compress_effective()),
                        "←→ cycle",
                    ),
                    ConfirmField::CompressPassword => {
                        let raw = ov
                            .compress_password
                            .clone()
                            .or_else(|| cfg.and_then(|c| c.compress_password.clone()))
                            .unwrap_or_default();
                        ("Zip pass", mask(&raw), "Enter edit  Tab show")
                    }
                    ConfirmField::Verify => (
                        "Verify",
                        on_off(ov.verify.unwrap_or(cfg.map(|c| c.verify).unwrap_or(false)))
                            .to_string(),
                        "←→ toggle",
                    ),
                    ConfirmField::NzbPassword => {
                        let raw = ov
                            .nzb_password
                            .clone()
                            .or_else(|| cfg.and_then(|c| c.nzb_password.clone()))
                            .unwrap_or_default();
                        ("NZB pass", mask(&raw), "Enter edit  Tab show")
                    }
                    ConfirmField::Groups => (
                        "Groups",
                        ov.groups
                            .clone()
                            .or_else(|| cfg.map(|c| c.groups.join(", ")))
                            .unwrap_or_else(|| UNSET.to_string()),
                        "Enter edit",
                    ),
                    ConfirmField::From => (
                        "From",
                        ov.from
                            .clone()
                            .or_else(|| cfg.map(|c| c.from.clone()))
                            .unwrap_or_else(|| UNSET.to_string()),
                        "Enter edit",
                    ),
                    ConfirmField::Category => (
                        "Category",
                        ov.nzb_category
                            .clone()
                            .or_else(|| cfg.and_then(|c| c.nzb_category.clone()))
                            .unwrap_or_else(|| UNSET.to_string()),
                        "Enter edit",
                    ),
                    ConfirmField::ArticleSize => {
                        let kb = ov
                            .article_size_kb
                            .unwrap_or(cfg.map(|c| c.article_size / 1024).unwrap_or(768));
                        ("Article", format!("{kb} KB"), "Enter edit")
                    }
                };
                ConfirmFieldView { label, value, hint }
            })
            .collect()
    }

    /// One-line explanation of the current obfuscation mode, for the panel.
    pub fn obfuscate_legend(&self) -> &'static str {
        match self.obf_effective() {
            ObfuscateMode::None => "None: public subject + real filenames",
            ObfuscateMode::Subject => "Subject: random subject, real poster + filenames",
            ObfuscateMode::Full => "Full: random subject + poster + filenames",
        }
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

    /// Persist current session overrides to disk so they are pre-filled next time.
    pub fn save_upload_prefs(&self) {
        if let Some(path) = upload_prefs_path() {
            if let Ok(json) = serde_json::to_string_pretty(&self.config_state.overrides) {
                let _ = std::fs::write(path, json);
            }
        }
    }

    /// Persist the current upload queue (the list of paths) so a carefully
    /// built selection survives navigating away or restarting the app.
    pub fn save_queue(&self) {
        if let Some(path) = queue_path() {
            if let Ok(json) = serde_json::to_string_pretty(&self.upload_queue.items) {
                let _ = std::fs::write(path, json);
            }
        }
    }

    /// Restore a previously saved queue, dropping any path that no longer
    /// exists on disk, and rebuild the grouping cache and Browser badges.
    pub fn load_queue(&mut self) {
        let Some(path) = queue_path() else { return };
        let Ok(data) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(items) = serde_json::from_str::<Vec<String>>(&data) else {
            return;
        };
        let mut dropped = 0usize;
        self.upload_queue.items = items
            .into_iter()
            .filter(|p| {
                let exists = std::path::Path::new(p).exists();
                if !exists {
                    dropped += 1;
                }
                exists
            })
            .collect();
        self.upload_queue.selected = 0;
        // Quick (walk-free) restore: folders come back unsized and their
        // file count / size is computed off the UI thread, so restoring a queue
        // of large folders cannot stall startup.
        self.queue_meta = self
            .upload_queue
            .items
            .iter()
            .map(|p| (p.clone(), queue_entry_info_quick(p)))
            .collect();
        self.pending_meta = self
            .queue_meta
            .iter()
            .filter(|(_, info)| !info.sized)
            .map(|(p, _)| p.clone())
            .collect();
        self.sync_queue_badges();
        let n = self.upload_queue.items.len();
        if n > 0 {
            let mut msg = format!("Restored {n} queued item(s)");
            if dropped > 0 {
                msg.push_str(&format!(" ({dropped} missing path(s) dropped)"));
            }
            self.status_bar.set(msg);
        }
        // Persist the pruned list so missing paths do not linger.
        if dropped > 0 {
            self.save_queue();
        }
    }

    /// Load previously saved session overrides and merge them into config_state.
    /// Values already set (e.g. from the pesto config) are not overwritten.
    pub fn load_upload_prefs(&mut self) {
        let Some(path) = upload_prefs_path() else {
            return;
        };
        let Ok(data) = std::fs::read_to_string(path) else {
            return;
        };
        if let Ok(prefs) = serde_json::from_str::<SessionOverrides>(&data) {
            let o = &mut self.config_state.overrides;
            if o.obfuscate.is_none() {
                o.obfuscate = prefs.obfuscate;
            }
            if o.par2.is_none() {
                o.par2 = prefs.par2;
            }
            if o.verify.is_none() {
                o.verify = prefs.verify;
            }
            if o.nzb_password.is_none() {
                o.nzb_password = prefs.nzb_password;
            }
            if o.groups.is_none() {
                o.groups = prefs.groups;
            }
            if o.compress_password.is_none() {
                o.compress_password = prefs.compress_password;
            }
            if o.compress_format.is_none() {
                o.compress_format = prefs.compress_format;
            }
            if o.from.is_none() {
                o.from = prefs.from;
            }
            if o.nzb_category.is_none() {
                o.nzb_category = prefs.nzb_category;
            }
            if o.article_size_kb.is_none() {
                o.article_size_kb = prefs.article_size_kb;
            }
            // folder_mode is intentionally NOT restored: it is a per-batch choice
            // (defaults to a single release NZB each session) so an old "season"
            // selection can never silently change how a folder uploads later.
        }
    }
}

/// Recursively collect all `.nzb` files under `dir` into `out`.
///
/// Origin is derived from the immediate parent directory name relative to the
/// scan root: `uploaded` → Uploaded, `downloaded` → Downloaded, all others
/// (including the root itself) → Manual.
fn collect_nzbs_recursive(
    dir: &std::path::Path,
    catalog_paths: &std::collections::HashSet<String>,
    out: &mut Vec<VaultEntry>,
) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_nzbs_recursive(&path, catalog_paths, out);
            continue;
        }
        if !path
            .extension()
            .map(|x| x.eq_ignore_ascii_case("nzb"))
            .unwrap_or(false)
        {
            continue;
        }
        let origin = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|n| match n {
                "uploaded" => NzbOrigin::Uploaded,
                "downloaded" => NzbOrigin::Downloaded,
                _ => NzbOrigin::Manual,
            })
            .unwrap_or(NzbOrigin::Manual);
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let meta = entry.metadata().ok();
        let file_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let in_catalog = catalog_paths.contains(&path.to_string_lossy().to_string());
        out.push(VaultEntry {
            path,
            name,
            file_size,
            modified,
            contents: None,
            in_catalog,
            origin,
        });
    }
}

/// Recursively scan `dir` for `.nzb` files and map each file's release key
/// (see `file_tree::release_key`) to its [`DiskNzbInfo`] (origin + password).
/// Capped so a pathological tree cannot stall startup; symlinks are skipped to
/// match the rest of the walk logic.
fn collect_nzb_release_keys(
    dir: &std::path::Path,
    out: &mut std::collections::HashMap<String, DiskNzbInfo>,
) {
    const CAP: usize = 200_000;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        // Origin is derived from the immediate parent directory name, matching
        // `collect_nzbs_recursive` and `prowlarr::dest_path_in` (downloaded/).
        let origin = d
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| match n {
                "uploaded" => NzbOrigin::Uploaded,
                "downloaded" => NzbOrigin::Downloaded,
                _ => NzbOrigin::Manual,
            })
            .unwrap_or(NzbOrigin::Manual);
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            } else if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                let name = entry.file_name();
                let is_nzb = std::path::Path::new(&name)
                    .extension()
                    .map(|x| x.eq_ignore_ascii_case("nzb"))
                    .unwrap_or(false);
                if is_nzb {
                    if let Some(n) = name.to_str() {
                        let info = DiskNzbInfo {
                            origin,
                            has_password: nzb_head_has_password(&entry.path()),
                        };
                        out.entry(crate::ui::components::file_tree::release_key(n))
                            // On a duplicate release key, prefer a non-Manual
                            // origin and keep a password flag once seen.
                            .and_modify(|e| {
                                if e.origin == NzbOrigin::Manual {
                                    e.origin = info.origin;
                                }
                                e.has_password |= info.has_password;
                            })
                            .or_insert(info);
                    }
                    if out.len() >= CAP {
                        return;
                    }
                }
            }
        }
    }
}

/// Cheap password probe: read only the head of an `.nzb` (the `<head>`/`<meta>`
/// block always precedes the `<file>` entries) and look for a
/// `<meta type="password">` tag. Avoids parsing whole NZBs, which can be huge.
fn nzb_head_has_password(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 4096];
    let n = f.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.contains("type=\"password\"") || head.contains("type='password'")
}

/// Path to the upload preferences file.
fn upload_prefs_path() -> Option<PathBuf> {
    pesto::config::config_dir().map(|d| d.join("upapasta-prefs.json"))
}

/// Path to the persisted upload queue.
fn queue_path() -> Option<PathBuf> {
    pesto::config::config_dir().map(|d| d.join("upapasta-queue.json"))
}

/// Insert, update, or remove `[output.indexer].<field>` in a config.toml
/// document, preserving everything else (comments, ordering, formatting).
///
/// A `Some(value)` writes/updates the key; `None` removes it. The `[output]` /
/// `[output.indexer]` parents are vivified as *implicit regular* tables (see
/// [`ensure_implicit_table`]) so they render as `[output.indexer]` headers
/// rather than inline tables — empty parents are never emitted as bare headers,
/// and a later removal can find the key. Returns the new document text, or a
/// parse error if the input is not valid TOML.
fn apply_indexer_field(
    text: &str,
    field: &str,
    value: Option<&str>,
) -> Result<String, toml_edit::TomlError> {
    use toml_edit::Item;
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    match value {
        Some(v) => {
            let output = ensure_implicit_table(doc.as_table_mut(), "output");
            let indexer = ensure_implicit_table(output, "indexer");
            indexer.insert(field, toml_edit::value(v));
        }
        None => {
            if let Some(output) = doc.get_mut("output").and_then(Item::as_table_mut) {
                if let Some(indexer) = output.get_mut("indexer").and_then(Item::as_table_mut) {
                    indexer.remove(field);
                }
            }
        }
    }
    Ok(doc.to_string())
}

/// Return a mutable reference to `parent[key]` as a regular table, creating it
/// as an *implicit* table when absent (or when the slot holds a non-table, e.g.
/// an inline table). Implicit means the empty header is suppressed, so a nested
/// child like `[output.indexer]` does not drag a bare `[output]` header along.
fn ensure_implicit_table<'a>(
    parent: &'a mut toml_edit::Table,
    key: &str,
) -> &'a mut toml_edit::Table {
    if !parent
        .get(key)
        .map(toml_edit::Item::is_table)
        .unwrap_or(false)
    {
        let mut tbl = toml_edit::Table::new();
        tbl.set_implicit(true);
        parent.insert(key, toml_edit::Item::Table(tbl));
    }
    parent[key].as_table_mut().expect("just ensured table")
}

/// Expand a leading `~` to the user's home directory.
/// Paths without `~` are returned as-is.
pub fn expand_tilde(path: &str) -> PathBuf {
    let home = std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| directories::UserDirs::new().map(|u| u.home_dir().to_path_buf()));
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(h) = home {
            return h.join(rest);
        }
    } else if path == "~" {
        if let Some(h) = home {
            return h;
        }
    }
    PathBuf::from(path)
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

#[cfg(test)]
mod tests {
    use super::{apply_indexer_field, queue_entry_info, queue_entry_info_quick};
    use std::fs;

    fn indexer_str(doc_text: &str, field: &str) -> Option<String> {
        let doc = doc_text
            .parse::<toml_edit::DocumentMut>()
            .expect("output is valid TOML");
        // Read with `get` chaining: indexing a regular `Table` with a missing
        // key panics, and a removed field is legitimately absent.
        doc.get("output")
            .and_then(|o| o.get("indexer"))
            .and_then(|i| i.get(field))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// Writing into an empty (or missing) config creates `[output.indexer]`
    /// with the field, and the result is valid TOML.
    #[test]
    fn indexer_field_written_into_empty_config() {
        let out = apply_indexer_field("", "url", Some("http://localhost:9696")).unwrap();
        assert_eq!(
            indexer_str(&out, "url").as_deref(),
            Some("http://localhost:9696")
        );
        // No bare empty `[output]` header should precede the nested table.
        assert!(
            !out.contains("[output]\n"),
            "unexpected bare header:\n{out}"
        );
    }

    /// Existing keys and comments elsewhere in the file are preserved, and a new
    /// `[output.indexer]` field is added alongside an existing one.
    #[test]
    fn indexer_field_preserves_rest_of_config() {
        let original = "\
# my config
[server]
host = \"news.example.com\" # keep me

[output]
nzb_dir = \"~/nzb\"

[output.indexer]
url = \"http://old:9696\"
";
        let out = apply_indexer_field(original, "api_key", Some("secret123")).unwrap();
        // Comment and unrelated keys survive verbatim.
        assert!(out.contains("# my config"));
        assert!(out.contains("host = \"news.example.com\" # keep me"));
        assert!(out.contains("nzb_dir = \"~/nzb\""));
        // Both the pre-existing url and the new api_key are present.
        assert_eq!(indexer_str(&out, "url").as_deref(), Some("http://old:9696"));
        assert_eq!(indexer_str(&out, "api_key").as_deref(), Some("secret123"));
    }

    /// Writing the same field twice updates in place rather than duplicating it.
    #[test]
    fn indexer_field_updates_in_place() {
        let step1 = apply_indexer_field("", "url", Some("http://a:1")).unwrap();
        let step2 = apply_indexer_field(&step1, "url", Some("http://b:2")).unwrap();
        assert_eq!(
            step2.matches("url =").count(),
            1,
            "url duplicated:\n{step2}"
        );
        assert_eq!(indexer_str(&step2, "url").as_deref(), Some("http://b:2"));
    }

    /// A `None` value removes the field (clearing it in the Config screen).
    #[test]
    fn indexer_field_removed_when_none() {
        let with = apply_indexer_field("", "api_key", Some("secret")).unwrap();
        let without = apply_indexer_field(&with, "api_key", None).unwrap();
        assert_eq!(indexer_str(&without, "api_key"), None);
    }

    /// The quick form must not walk a directory: it returns immediately with
    /// `sized: false` and zeroed counts so the UI thread never blocks. The full
    /// form then fills in the real numbers via the recursive walk.
    #[test]
    fn quick_info_defers_folder_sizing() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(dir.path().join("a.bin"), [0u8; 100]).unwrap();
        fs::write(sub.join("b.bin"), [0u8; 200]).unwrap();
        let path = dir.path().to_string_lossy().to_string();

        let quick = queue_entry_info_quick(&path);
        assert!(quick.is_dir);
        assert!(!quick.sized);
        assert_eq!(quick.file_count, 0);
        assert_eq!(quick.size_bytes, 0);
        assert_eq!(quick.files_label(), "…");

        // The full form walks the tree (2 files, 300 bytes) and is marked sized.
        let full = queue_entry_info(&path);
        assert!(full.sized);
        assert_eq!(full.file_count, 2);
        assert_eq!(full.size_bytes, 300);
        assert_eq!(full.files_label(), "2");
    }

    /// The aggregate bar must track each queue item, not stay pinned at the
    /// previous item's 100%. `apply` only grows the counters within one item, so
    /// `reset_for_item` is what lets the next item's smaller counts show.
    #[test]
    fn progress_bar_tracks_each_queue_item() {
        use super::UploadProgress;
        use crate::events::ProgressUpdate;

        fn upd(done_segments: u64, total_segments: u64) -> ProgressUpdate {
            ProgressUpdate {
                done_segments,
                total_segments,
                done_bytes: done_segments * 1000,
                total_bytes: total_segments * 1000,
                current_speed_mbps: 0.0,
                message: None,
                file_update: None,
                phase: None,
                par2_slices: None,
                queue_extended: None,
                par2_hint_bytes: 0,
                par2_complete: false,
            }
        }

        let mut p = UploadProgress::default();
        // Item 1 runs to completion.
        p.apply(&upd(100, 100));
        assert_eq!(p.done_segments, 100);
        assert_eq!(p.total_segments, 100);

        // Without the reset, item 2's smaller counts (5 < 100) would be ignored
        // by the monotonic apply and the bar would stay at 100%.
        p.reset_for_item();
        assert_eq!(p.done_segments, 0);
        assert_eq!(p.total_segments, 0);

        p.apply(&upd(5, 50));
        assert_eq!(p.done_segments, 5);
        assert_eq!(p.total_segments, 50);
        assert!((p.progress_pct() - 10.0).abs() < 1e-9);
    }

    /// A plain file is fully resolved by the quick form (a single `stat`), so it
    /// never needs a background job.
    #[test]
    fn quick_info_resolves_plain_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("movie.mkv");
        fs::write(&file, [0u8; 42]).unwrap();

        let info = queue_entry_info_quick(&file.to_string_lossy());
        assert!(!info.is_dir);
        assert!(info.sized);
        assert_eq!(info.nzb_name, "movie");
        assert_eq!(info.size_bytes, 42);
    }
}
