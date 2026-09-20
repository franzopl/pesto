use crate::catalog::Catalog;
use crate::events::{ProgressUpdate, UploadPhase};
use crate::ui::components::{FileTree, LogPanel, StatusBar, UploadQueue};
use pesto::config::{Config as PestoConfig, FileConfig, ObfuscateMode};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

mod config;
mod confirm;
mod history;
mod hook_picker;
mod navigation;
mod prowlarr;
mod queue;
mod upload;
mod vault;
mod watch;

pub use config::{ConfigState, FolderMode};
pub use history::HistoryState;
pub use hook_picker::HookPickerState;
pub use navigation::AppState;
pub use prowlarr::{ProwlarrBatchState, ProwlarrSearchState, ProwlarrState};
pub(crate) use queue::dir_stats;
pub use queue::{queue_entry_info, QueueEntryInfo};
pub use vault::{DiskNzbInfo, NzbOrigin, VaultEntry, VaultSort, VaultState};
pub use watch::WatchState;

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
    /// True while the user has paused the upload (`p` on the Dashboard).
    /// Posting workers suspend at the next segment-batch boundary; PAR2,
    /// compression and the check/repost passes are unaffected — see
    /// `pesto::poster::post_files_inner`'s doc for the same scoping `cancel`
    /// already has.
    pub is_paused: bool,

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

    /// Streaming check queue progress (runs concurrently with NNTP posting,
    /// for the lifetime of the upload rather than as its own phase).
    pub check_checked: u64,
    pub check_failed: u64,

    /// Bytes pre-seeded from par2_bytes_hint; consumed as QueueExtended arrives
    /// so total_bytes never jumps backwards.
    pub par2_hint_remaining: u64,
    /// Segments pre-seeded from par2_segments_hint; consumed as QueueExtended
    /// arrives, mirroring par2_hint_remaining for bytes.
    pub par2_segment_hint_remaining: u64,

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
    pub check: String,
}

// ── Canonical display vocabulary ──────────────────────────────────────────────
//
// One source of truth for how settings are *shown*, so the Dashboard summary,
// the upload-config panel and the Config overrides all read the same. These map
// internal values to display labels only — the stored values (enums, bools and
// the compress-format token used by the cycle handlers) are untouched.

/// Display label for an obfuscation mode.
pub fn obf_label(mode: ObfuscateMode) -> &'static str {
    match mode {
        ObfuscateMode::None => "None",
        ObfuscateMode::Full => "Full",
        ObfuscateMode::FullShared => "Full (shared)",
        ObfuscateMode::Light => "Light (shared, matching)",
        ObfuscateMode::Article => "Article (experimental)",
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
        self.check_checked = 0;
        self.check_failed = 0;
        self.par2_hint_remaining = 0;
        self.par2_segment_hint_remaining = 0;
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
            // Absorb the real PAR2 bytes/segments against the pre-seeded
            // hints so neither total jumps (same logic as pesto's terminal
            // renderer) — only the excess over the hint grows the total.
            if bytes <= self.par2_hint_remaining {
                self.par2_hint_remaining -= bytes;
            } else {
                let excess = bytes - self.par2_hint_remaining;
                self.par2_hint_remaining = 0;
                self.total_bytes += excess;
            }
            if seg <= self.par2_segment_hint_remaining {
                self.par2_segment_hint_remaining -= seg;
            } else {
                let excess = seg - self.par2_segment_hint_remaining;
                self.par2_segment_hint_remaining = 0;
                self.total_segments += excess;
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
        if let Some((checked, failed)) = update.check_progress {
            self.check_checked = checked;
            self.check_failed = failed;
        }
        if update.par2_hint_bytes > 0 {
            self.par2_hint_remaining = update.par2_hint_bytes;
        }
        if update.par2_segment_hint > 0 {
            self.par2_segment_hint_remaining = update.par2_segment_hint;
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
    /// Shared with the in-flight upload task via `pesto::upload::run_upload`'s
    /// `pause` parameter. Set to `true`/`false` by `toggle_pause_upload`;
    /// dropped (set back to `None`) when the upload ends.
    pub current_pause_flag: Option<Arc<AtomicBool>>,

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

    /// Watch-mode state (monitored directory, stability tracking, toggle).
    pub watch: WatchState,

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

    /// Hook picker overlay: when set, the user is choosing which hook to run
    /// against the selected release (Browser `r`). `None` = overlay closed.
    pub hook_picker: Option<HookPickerState>,
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
            current_pause_flag: None,
            pesto_config,
            config_path,
            config_error,
            catalog,
            history: HistoryState::default(),
            upload_started_at: None,
            config_state: ConfigState::default(),
            watch: WatchState::default(),
            show_upload_confirm: false,
            confirm_field: 0,
            confirm_editing: false,
            confirm_edit_buf: String::new(),
            confirm_show_password: false,
            vault: VaultState::default(),
            prowlarr: ProwlarrState::default(),
            hook_picker: None,
        };
        app.load_watch_settings();
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
        // Flag releases already sent through a hook (e.g. an indexer upload).
        app.refresh_hooked_index();

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

/// Locate an `.nzb` on disk whose release key matches `release_name`.
///
/// The manual "run hooks" action selects a media release in the Browser; we
/// resolve it to its NZB in `nzb_dir` the same way the disk-index badge does —
/// by [`release_key`](crate::ui::components::file_tree::release_key) — so a
/// season pack folder maps to its season `.nzb`. Returns the first match.
/// Capped and symlink-skipping like the rest of the walk logic.
pub(crate) fn find_nzb_for_release(
    nzb_dir: &std::path::Path,
    release_name: &str,
) -> Option<PathBuf> {
    use crate::ui::components::file_tree::release_key;
    let target = release_key(release_name);
    if target.is_empty() {
        return None;
    }
    const CAP: usize = 200_000;
    let mut stack = vec![nzb_dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(d) = stack.pop() {
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
                visited += 1;
                let name = entry.file_name();
                let is_nzb = std::path::Path::new(&name)
                    .extension()
                    .map(|x| x.eq_ignore_ascii_case("nzb"))
                    .unwrap_or(false);
                if is_nzb {
                    if let Some(n) = name.to_str() {
                        if release_key(n) == target {
                            return Some(entry.path());
                        }
                    }
                }
                if visited >= CAP {
                    return None;
                }
            }
        }
    }
    None
}

/// Find a `.nfo` sitting next to `nzb_path` so it can be handed to hooks via
/// `PESTO_NFO`. Checks `<stem>.nfo` (extension swapped) then `<full-name>.nfo`.
pub(crate) fn find_sibling_nfo(nzb_path: &std::path::Path) -> Option<PathBuf> {
    let with_nfo = nzb_path.with_extension("nfo");
    if with_nfo.is_file() {
        return Some(with_nfo);
    }
    let mut alt = nzb_path.as_os_str().to_owned();
    alt.push(".nfo");
    let alt = PathBuf::from(alt);
    if alt.is_file() {
        return Some(alt);
    }
    None
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

/// Path to the persisted watch-mode settings.
fn watch_settings_path() -> Option<PathBuf> {
    pesto::config::config_dir().map(|d| d.join("upapasta-watch.json"))
}

/// Fold a background scan's (path, size) snapshot into `watch`'s stability
/// tracker, returning the log lines the caller should display as `(message,
/// is_warn)`. The first scan of a directory only baselines it (marks every
/// current entry as already-seen, without queuing anything) — see
/// `WatchState::baseline_captured`. Later scans queue an entry once its size
/// stops changing across two consecutive scans, mirroring the settle check
/// `pesto --watch` uses.
fn fold_watch_scan(watch: &mut WatchState, entries: &[(PathBuf, u64)]) -> Vec<(String, bool)> {
    let mut lines = Vec::new();

    if !watch.baseline_captured {
        watch.baseline_captured = true;
        for (path, _) in entries {
            watch.seen.insert(path.clone());
        }
        if !entries.is_empty() {
            lines.push((
                format!(
                    "[watch] baseline captured — {} existing item(s) ignored",
                    entries.len()
                ),
                false,
            ));
        }
        return lines;
    }

    let done_dir = watch.done_dir.clone();
    for (path, size) in entries {
        if watch.seen.contains(path) {
            continue;
        }
        if done_dir.as_ref().is_some_and(|done| path.starts_with(done)) {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        match watch.pending.get(path).copied() {
            None => {
                watch.pending.insert(path.clone(), *size);
                lines.push((
                    format!("[watch] detected {name} — waiting to stabilize"),
                    false,
                ));
            }
            Some(prev) if prev != *size => {
                watch.pending.insert(path.clone(), *size);
            }
            Some(prev) => {
                watch.pending.remove(path);
                watch.seen.insert(path.clone());
                if prev == 0 {
                    lines.push((format!("[watch] {name}: empty, ignoring"), true));
                } else {
                    watch.ready.push_back(path.clone());
                    lines.push((
                        format!("[watch] {name} is stable — queued for upload"),
                        false,
                    ));
                }
            }
        }
    }

    // Drop stability tracking for anything that vanished before settling.
    let present: std::collections::HashSet<&PathBuf> = entries.iter().map(|(p, _)| p).collect();
    watch.pending.retain(|p, _| present.contains(p));

    lines
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
    match pesto::config::default_config_path() {
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
    use super::queue::queue_entry_info_quick;
    use super::{apply_indexer_field, queue_entry_info};
    use std::fs;

    mod watch_scan {
        use super::super::{fold_watch_scan, WatchState};
        use std::path::PathBuf;

        fn p(name: &str) -> PathBuf {
            PathBuf::from(format!("/watch/{name}"))
        }

        /// The first scan of a directory only baselines it: every entry
        /// already present is marked seen but nothing is queued, matching
        /// the "ignore what already exists" behavior watch mode promises.
        #[test]
        fn first_scan_baselines_without_queuing() {
            let mut w = WatchState::default();
            let lines = fold_watch_scan(&mut w, &[(p("a.mkv"), 100), (p("b.mkv"), 200)]);

            assert!(w.baseline_captured);
            assert!(w.ready.is_empty());
            assert!(w.pending.is_empty());
            assert!(w.seen.contains(&p("a.mkv")));
            assert!(w.seen.contains(&p("b.mkv")));
            assert_eq!(lines.len(), 1);
            assert!(lines[0].0.contains("baseline captured"));
        }

        /// A brand-new entry (after baselining) is tracked as pending on its
        /// first sighting, then queued only once its size repeats unchanged.
        #[test]
        fn new_entry_queues_once_size_is_stable_across_two_scans() {
            let mut w = WatchState::default();
            fold_watch_scan(&mut w, &[]); // baseline: nothing pre-existing

            fold_watch_scan(&mut w, &[(p("new.mkv"), 100)]);
            assert!(w.ready.is_empty(), "queued before stabilizing");
            assert_eq!(w.pending.get(&p("new.mkv")), Some(&100));

            fold_watch_scan(&mut w, &[(p("new.mkv"), 100)]);
            assert_eq!(w.ready.into_iter().collect::<Vec<_>>(), vec![p("new.mkv")]);
            assert!(w.pending.is_empty());
            assert!(w.seen.contains(&p("new.mkv")));
        }

        /// A still-growing file must never be queued: each differing size
        /// just re-arms the settle check instead.
        #[test]
        fn still_changing_size_is_never_queued() {
            let mut w = WatchState::default();
            fold_watch_scan(&mut w, &[]);

            fold_watch_scan(&mut w, &[(p("f.mkv"), 100)]);
            fold_watch_scan(&mut w, &[(p("f.mkv"), 150)]);
            fold_watch_scan(&mut w, &[(p("f.mkv"), 200)]);

            assert!(w.ready.is_empty());
            assert_eq!(w.pending.get(&p("f.mkv")), Some(&200));
        }

        /// A stable empty file/dir is ignored (marked seen) rather than
        /// queued for a pointless upload.
        #[test]
        fn stable_empty_entry_is_ignored_not_queued() {
            let mut w = WatchState::default();
            fold_watch_scan(&mut w, &[]);

            fold_watch_scan(&mut w, &[(p("empty.txt"), 0)]);
            let lines = fold_watch_scan(&mut w, &[(p("empty.txt"), 0)]);

            assert!(w.ready.is_empty());
            assert!(w.seen.contains(&p("empty.txt")));
            assert!(lines.iter().any(|(_, is_warn)| *is_warn));
        }

        /// An entry inside `done_dir` (watch mode's own output) must never
        /// be picked up, even once stable — otherwise a move-to-done would
        /// feed straight back into the watch loop.
        #[test]
        fn entries_inside_done_dir_are_never_queued() {
            let mut w = WatchState {
                done_dir: Some(PathBuf::from("/watch/done")),
                ..Default::default()
            };
            fold_watch_scan(&mut w, &[]);

            fold_watch_scan(&mut w, &[(PathBuf::from("/watch/done/old.mkv"), 100)]);
            fold_watch_scan(&mut w, &[(PathBuf::from("/watch/done/old.mkv"), 100)]);

            assert!(w.ready.is_empty());
            assert!(w.pending.is_empty());
        }

        /// An entry that disappears before settling (e.g. renamed or
        /// deleted) drops out of `pending` instead of lingering forever.
        #[test]
        fn vanished_entry_is_dropped_from_pending() {
            let mut w = WatchState::default();
            fold_watch_scan(&mut w, &[]);

            fold_watch_scan(&mut w, &[(p("gone.mkv"), 100)]);
            assert!(w.pending.contains_key(&p("gone.mkv")));

            fold_watch_scan(&mut w, &[]);
            assert!(w.pending.is_empty());
        }
    }

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
                check_progress: None,
                queue_extended: None,
                par2_hint_bytes: 0,
                par2_segment_hint: 0,
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
