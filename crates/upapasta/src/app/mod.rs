//! Application state facade.
//!
//! Screen-specific state and behavior live in the sibling modules below;
//! [`progress`] owns upload progress models and display vocabulary. This file
//! retains the shared [`App`] aggregate plus configuration-loading helpers.

use crate::catalog::Catalog;
use crate::ui::components::{FileTree, LogPanel, StatusBar, UploadQueue};
use pesto::config::{Config as PestoConfig, FileConfig};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

mod config;
mod confirm;
mod history;
mod hook_picker;
mod navigation;
mod progress;
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

pub use progress::{
    compress_label, obf_label, on_off, FileProgress, FileStatus, UploadProgress,
    UploadSettingsSummary, UNSET,
};

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
    /// thread, and folds the result back via [`App::apply_queue_meta`], so marking a
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
mod tests;
