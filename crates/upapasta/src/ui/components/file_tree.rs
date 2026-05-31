use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

use crate::app::{DiskNzbInfo, NzbOrigin};
use crate::catalog::NzbStatusEntry;

/// How a file appears in the browser based on its catalog/queue state.
#[derive(Debug, Clone)]
pub enum NzbBadge {
    /// Not in catalog, not queued.
    None,
    /// Queued for upload (Space key). The queue is the single source of truth;
    /// this badge is a render mirror of `App::upload_queue`.
    Marked,
    /// Currently being uploaded.
    Uploading,
    /// In catalog — carries the status entry.
    Uploaded(NzbStatusEntry),
    /// Not in the catalog, but a matching `.nzb` already exists in `nzb_dir`
    /// (matched by release name). Carries the on-disk origin so a Prowlarr
    /// download is distinguished from a prior upload, plus the password flag.
    OnDisk {
        origin: NzbOrigin,
        has_password: bool,
    },
}

#[derive(Debug)]
pub struct FileTree {
    /// Items currently visible (after the optional "unbacked only" filter).
    pub items: Vec<PathBuf>,
    /// Every entry in `current_dir` (before filtering); the source for `items`
    /// and for the directory summary line.
    all_items: Vec<PathBuf>,
    pub current_dir: PathBuf,
    pub selected: usize,
    pub show_hidden: bool,
    /// When true, the browser hides items that already have an NZB (in the
    /// catalog), so only what still needs uploading is shown.
    pub filter_unbacked: bool,
    /// Directory summary, recomputed on refresh / catalog change.
    /// `(total items, unbacked items, total bytes still to upload)`.
    summary: (usize, usize, u64),
    /// Absolute paths currently in the upload queue. This is a render mirror of
    /// `App::upload_queue`, refreshed via [`set_queued`]; it is never mutated
    /// directly so the queue stays the single source of truth.
    pub queued: HashSet<PathBuf>,
    /// NZB status from the catalog, keyed by original_name (filename or full path).
    pub nzb_status: HashMap<String, NzbStatusEntry>,
    /// Release keys (see [`release_key`]) of every `.nzb` found in the
    /// configured `nzb_dir`, mapped to their [`DiskNzbInfo`] (origin + password).
    /// Lets the browser flag a file as already-backed when a matching NZB exists
    /// on disk even if the catalog has no record, and distinguish a Prowlarr
    /// download from a prior upload.
    pub nzb_disk_index: HashMap<String, DiskNzbInfo>,
    /// Names of files currently being uploaded (basename).
    pub uploading: HashSet<String>,
    /// First visible item index — managed manually to get correct scroll behaviour.
    scroll_offset: usize,
    /// Number of items that fit in the last rendered area; updated at render time.
    visible_height: usize,
    /// Per-item scan result `(backed, upload_size_bytes)`, keyed by item path.
    /// Computed off the UI thread (see [`DirScanJob`]) and delivered via
    /// [`apply_scan`]. A missing key means "not scanned yet".
    scan_cache: HashMap<PathBuf, (bool, u64)>,
    /// Monotonic scan id. A delivered scan is applied only if it still matches
    /// the current generation, so results for a directory we already left (or a
    /// stale `nzb_status`) are discarded.
    scan_generation: u64,
    /// Set whenever a fresh background scan is needed (after navigation or a
    /// catalog change); consumed by [`take_scan_job`].
    scan_pending: bool,
    /// False until the scan for the current generation has been applied. The
    /// summary line shows a "scanning…" hint until then.
    summary_ready: bool,
}

/// A directory scan handed off to a blocking worker. It owns a snapshot of the
/// item list and catalog status so the (recursive, blocking) filesystem walks
/// run entirely off the UI thread.
#[derive(Debug)]
pub struct DirScanJob {
    pub generation: u64,
    items: Vec<PathBuf>,
    nzb_status: HashMap<String, NzbStatusEntry>,
    nzb_disk_index: HashMap<String, DiskNzbInfo>,
}

impl DirScanJob {
    /// Run the blocking filesystem walk for every item. Safe to call on a
    /// blocking thread; it never touches the UI. Returns the per-item
    /// `(path, backed, upload_size)` triples plus the generation it was for.
    pub fn run(self) -> (u64, Vec<(PathBuf, bool, u64)>) {
        let results = self
            .items
            .into_iter()
            .map(|p| {
                let backed = path_is_backed(&p, &self.nzb_status, &self.nzb_disk_index);
                // Only unbacked items contribute to the "to upload" byte total,
                // so skip the (expensive) size walk for backed ones.
                let size = if backed { 0 } else { item_size(&p) };
                (p, backed, size)
            })
            .collect();
        (self.generation, results)
    }
}

impl FileTree {
    pub fn new() -> Self {
        let mut tree = Self {
            items: vec![],
            all_items: vec![],
            current_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            selected: 0,
            show_hidden: false,
            filter_unbacked: false,
            summary: (0, 0, 0),
            queued: HashSet::new(),
            nzb_status: HashMap::new(),
            nzb_disk_index: HashMap::new(),
            uploading: HashSet::new(),
            scroll_offset: 0,
            visible_height: 20,
            scan_cache: HashMap::new(),
            scan_generation: 0,
            scan_pending: false,
            summary_ready: false,
        };
        tree.refresh();
        tree
    }

    /// Replace the NZB status map (called after catalog refresh). The summary
    /// and the unbacked filter depend on it, so recompute both.
    pub fn set_nzb_status(&mut self, status: HashMap<String, NzbStatusEntry>) {
        self.nzb_status = status;
        // Backed status depends on the catalog, so the cache is now stale:
        // schedule a fresh background scan instead of walking here.
        self.invalidate_scan();
        self.recompute_summary();
        self.apply_filter();
    }

    /// Replace the on-disk NZB release index (release keys of every `.nzb` in
    /// `nzb_dir`). Like the catalog map, backed status depends on it, so the
    /// scan cache is invalidated and a fresh background scan is scheduled.
    pub fn set_nzb_disk_index(&mut self, index: HashMap<String, DiskNzbInfo>) {
        self.nzb_disk_index = index;
        self.invalidate_scan();
        self.recompute_summary();
        self.apply_filter();
    }

    /// Mark names that are currently being uploaded.
    pub fn set_uploading(&mut self, names: HashSet<String>) {
        self.uploading = names;
    }

    /// Replace the set of queued paths (called whenever the upload queue
    /// changes). Keeps the `[x]` badge in the Browser in lock-step with the
    /// queue panel — one selection model, two views.
    pub fn set_queued(&mut self, paths: HashSet<PathBuf>) {
        self.queued = paths;
    }

    pub fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if self.selected + 1 >= self.items.len() {
            // Wrap to top.
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected += 1;
            // Scroll only when cursor leaves the visible area.
            let bottom = self.scroll_offset + self.visible_height;
            if self.selected >= bottom {
                self.scroll_offset = self.selected + 1 - self.visible_height;
            }
        }
    }

    pub fn select_previous(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if self.selected == 0 {
            // Wrap to bottom.
            self.selected = self.items.len() - 1;
            self.scroll_offset = self.items.len().saturating_sub(self.visible_height);
        } else {
            self.selected -= 1;
            // Scroll only when cursor leaves the visible area.
            if self.selected < self.scroll_offset {
                self.scroll_offset = self.selected;
            }
        }
    }

    pub fn get_selected(&self) -> Option<&PathBuf> {
        self.items.get(self.selected)
    }

    /// Return the NZB badge for the currently selected item.
    pub fn selected_badge(&self) -> Option<NzbBadge> {
        let path = self.items.get(self.selected)?;
        Some(self.badge_for(path))
    }

    /// Return the NZB badge for a given path.
    pub fn badge_for(&self, path: &PathBuf) -> NzbBadge {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let full = path.to_string_lossy();

        // Uploading takes precedence: an item stays in the queue while it is
        // being posted, so the live ▶ badge must win over the queued [x].
        if self.uploading.contains(name) {
            return NzbBadge::Uploading;
        }
        if self.queued.contains(path) {
            return NzbBadge::Marked;
        }
        if let Some(entry) = self
            .nzb_status
            .get(full.as_ref())
            .or_else(|| self.nzb_status.get(name))
        {
            return NzbBadge::Uploaded(entry.clone());
        }
        // Not in the catalog, but maybe a matching .nzb already exists in
        // nzb_dir (e.g. a Prowlarr download, or uploaded before this catalog).
        // A directory matches a release NZB named after the folder — a season
        // pack downloaded or uploaded as a single .nzb — by its own release key.
        if let Some(info) = self.nzb_disk_index.get(&release_key(name)) {
            return NzbBadge::OnDisk {
                origin: info.origin,
                has_password: info.has_password,
            };
        }
        NzbBadge::None
    }

    pub fn refresh(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.current_dir) {
            let mut items: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    if self.show_hidden {
                        true
                    } else {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| !s.starts_with('.'))
                            .unwrap_or(false)
                    }
                })
                .collect();

            items.sort_by(|a, b| {
                let a_is_dir = a.is_dir();
                let b_is_dir = b.is_dir();
                if a_is_dir != b_is_dir {
                    b_is_dir.cmp(&a_is_dir)
                } else {
                    a.file_name().cmp(&b.file_name())
                }
            });

            self.all_items = items;
            // The listing changed: the cached backed/size info no longer
            // matches, so request a fresh off-thread scan. `recompute_summary`
            // and `apply_filter` stay cheap (cache lookups); the real numbers
            // arrive later via `apply_scan`.
            self.invalidate_scan();
            self.recompute_summary();
            self.apply_filter();
        }
    }

    /// Mark the current listing as needing a fresh background scan. Bumps the
    /// generation so any in-flight scan for the previous state is discarded.
    fn invalidate_scan(&mut self) {
        self.scan_generation = self.scan_generation.wrapping_add(1);
        self.scan_pending = true;
        self.summary_ready = false;
    }

    /// Hand off the pending directory scan, if any. The caller runs
    /// [`DirScanJob::run`] on a blocking thread and returns the result through
    /// [`apply_scan`]. Returns `None` when no scan is pending.
    pub fn take_scan_job(&mut self) -> Option<DirScanJob> {
        if !self.scan_pending {
            return None;
        }
        self.scan_pending = false;
        Some(DirScanJob {
            generation: self.scan_generation,
            items: self.all_items.clone(),
            nzb_status: self.nzb_status.clone(),
            nzb_disk_index: self.nzb_disk_index.clone(),
        })
    }

    /// Apply a completed background scan. Stale results (a newer navigation or
    /// catalog change bumped the generation) are ignored.
    pub fn apply_scan(&mut self, generation: u64, results: Vec<(PathBuf, bool, u64)>) {
        if generation != self.scan_generation {
            return;
        }
        self.scan_cache = results
            .into_iter()
            .map(|(p, backed, size)| (p, (backed, size)))
            .collect();
        self.summary_ready = true;
        self.recompute_summary();
        self.apply_filter();
    }

    /// Rebuild `items` from `all_items`, honoring the unbacked filter, and clamp
    /// the cursor/scroll to the new length.
    fn apply_filter(&mut self) {
        self.items = if self.filter_unbacked {
            self.all_items
                .iter()
                .filter(|p| !self.is_backed(p))
                .cloned()
                .collect()
        } else {
            self.all_items.clone()
        };
        if self.selected >= self.items.len() {
            self.selected = 0;
            self.scroll_offset = 0;
        }
    }

    /// Toggle the "show only items without an NZB" filter.
    pub fn toggle_filter_unbacked(&mut self) {
        self.filter_unbacked = !self.filter_unbacked;
        self.selected = 0;
        self.scroll_offset = 0;
        self.apply_filter();
    }

    /// Recompute the `(total, unbacked, bytes-to-upload)` summary for the
    /// current directory listing.
    ///
    /// Reads from `scan_cache`; an item not yet scanned counts as unbacked with
    /// zero size. While `summary_ready` is false the render path shows a
    /// "scanning…" hint instead of these provisional numbers.
    fn recompute_summary(&mut self) {
        let total = self.all_items.len();
        let mut unbacked = 0usize;
        let mut bytes = 0u64;
        for p in &self.all_items {
            match self.scan_cache.get(p) {
                Some((true, _)) => {}
                Some((false, size)) => {
                    unbacked += 1;
                    bytes += size;
                }
                None => unbacked += 1,
            }
        }
        self.summary = (total, unbacked, bytes);
    }

    /// `(total items, unbacked items, bytes still to upload)` for the status line.
    pub fn summary(&self) -> (usize, usize, u64) {
        self.summary
    }

    /// Whether a path is already backed up, from the most recent background
    /// scan. An item not yet scanned is treated as *not* backed so it stays
    /// visible (and counted) until the real result arrives.
    fn is_backed(&self, path: &Path) -> bool {
        self.scan_cache
            .get(path)
            .map(|(backed, _)| *backed)
            .unwrap_or(false)
    }

    pub fn go_to_parent(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.refresh();
            self.selected = 0;
            self.scroll_offset = 0;
        }
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refresh();
    }

    /// Render this FileTree into the given area.
    pub fn render(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        // Keep visible_height in sync so navigation knows how many rows fit.
        self.visible_height = (area.height as usize).saturating_sub(2).max(1);
        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let badge = self.badge_for(path);
                let is_dir = path.is_dir();
                let raw_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                // Directories carry a single-width marker + trailing slash so
                // they read at a glance without relying on (double-width) emoji.
                let marker = if is_dir {
                    crate::ui::theme::DIR_MARK
                } else {
                    crate::ui::theme::FILE_MARK
                };
                let name = if is_dir {
                    format!("{raw_name}/")
                } else {
                    raw_name.to_string()
                };
                let is_selected = i == self.selected;

                let (check, check_style, name_style) = match &badge {
                    NzbBadge::Marked => (
                        "[x] ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                        if is_selected {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Green)
                        },
                    ),
                    NzbBadge::Uploading => (
                        "[▶] ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Cyan),
                    ),
                    NzbBadge::Uploaded(entry) => {
                        let (sym, color) = badge_symbol(entry);
                        (
                            sym,
                            Style::default().fg(color).add_modifier(Modifier::DIM),
                            if is_selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(color).add_modifier(Modifier::DIM)
                            },
                        )
                    }
                    NzbBadge::OnDisk {
                        origin,
                        has_password,
                    } => {
                        let (sym, color) = disk_badge_symbol(*origin, *has_password);
                        (
                            sym,
                            Style::default().fg(color).add_modifier(Modifier::DIM),
                            if is_selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(color).add_modifier(Modifier::DIM)
                            },
                        )
                    }
                    NzbBadge::None => (
                        "[ ] ",
                        Style::default().fg(Color::DarkGray),
                        if is_selected {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else if is_dir {
                            Style::default().fg(Color::Blue)
                        } else {
                            Style::default()
                        },
                    ),
                };

                // Marker takes the directory accent unless the row is selected
                // (then the highlight bg owns the styling).
                let marker_style = if is_dir && !is_selected {
                    Style::default().fg(Color::Blue)
                } else {
                    name_style
                };

                ListItem::new(Line::from(vec![
                    Span::styled(check, check_style),
                    Span::styled(marker, marker_style),
                    Span::styled(name, name_style),
                ]))
            })
            .collect();

        let n_queued = self.queued.len();
        let queued_hint = if n_queued > 0 {
            format!(" — {} queued", n_queued)
        } else {
            String::new()
        };

        let (total, unbacked, bytes) = self.summary;
        let summary = if !self.summary_ready && total > 0 {
            " — scanning…".to_string()
        } else if unbacked > 0 {
            format!(" — {} unbacked · {} to upload", unbacked, fmt_bytes(bytes))
        } else if total > 0 {
            " — all backed ✓".to_string()
        } else {
            String::new()
        };
        let filter_tag = if self.filter_unbacked {
            " • filter:unbacked"
        } else {
            ""
        };

        let title = format!(
            " Browser — {} ({} items{}{}{}{}) ",
            self.current_dir.display(),
            total,
            if self.show_hidden { " • hidden" } else { "" },
            filter_tag,
            queued_hint,
            summary,
        );

        let border_style = if self.filter_unbacked {
            Style::default().fg(Color::Magenta)
        } else if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(border_style),
            )
            .highlight_style(crate::ui::theme::highlight());

        let mut state = ListState::default();
        state.select(Some(self.selected));
        *state.offset_mut() = self.scroll_offset;

        f.render_stateful_widget(list, area, &mut state);
    }
}

/// Returns `(badge_string, color)` for a catalog entry.
/// Badge is always 4 chars wide so the list stays aligned.
fn badge_symbol(entry: &NzbStatusEntry) -> (&'static str, Color) {
    match (entry.obfuscated, entry.has_password) {
        (false, false) => ("[✓] ", Color::Green),
        (true, false) => ("[~] ", Color::Yellow),
        (false, true) => ("[P] ", Color::Magenta),
        (true, true) => ("[*] ", Color::Cyan),
    }
}

/// Returns `(badge_string, color)` for an `.nzb` matched on disk but not in the
/// catalog. A password-protected release is always flagged with a magenta `P`
/// for maximum visibility; otherwise a Prowlarr download shows a yellow `↓` and
/// a prior upload (or manual file) a green `✓`. Badge stays 4 chars wide.
fn disk_badge_symbol(origin: NzbOrigin, has_password: bool) -> (&'static str, Color) {
    match (origin, has_password) {
        (NzbOrigin::Downloaded, false) => ("[↓] ", Color::Yellow),
        (NzbOrigin::Downloaded, true) => ("[↓P]", Color::Magenta),
        (_, false) => ("[✓] ", Color::Green),
        (_, true) => ("[✓P]", Color::Magenta),
    }
}

/// Compact human-readable byte size (e.g. `3.2 GB`) for the summary line.
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

/// Whether a path is already backed up (has an NZB in the catalog).
///
/// A file is backed when it is in the catalog by full path or base name. A
/// directory is backed when it was uploaded as a release (its folder name is in
/// the catalog) *or* every file under it is individually backed — i.e. it is
/// unbacked if any child still needs uploading. The directory case walks the
/// subtree, so this runs on a blocking worker (see [`DirScanJob::run`]), never
/// on the UI thread.
fn path_is_backed(
    path: &Path,
    nzb_status: &HashMap<String, NzbStatusEntry>,
    nzb_disk_index: &HashMap<String, DiskNzbInfo>,
) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let full = path.to_string_lossy();
    if nzb_status.contains_key(full.as_ref()) || nzb_status.contains_key(name) {
        return true;
    }
    if path.is_dir() {
        // A release NZB named after the folder (e.g. a downloaded season pack)
        // backs the whole directory; otherwise fall back to checking that every
        // inner file is individually backed.
        if nzb_disk_index.contains_key(&release_key(name)) {
            return true;
        }
        return !dir_has_unbacked(path, nzb_status, nzb_disk_index);
    }
    // A matching .nzb already on disk counts as backed.
    nzb_disk_index.contains_key(&release_key(name))
}

/// Whether `dir` contains at least one file (recursively) that is not in the
/// catalog by its base name. Walks with a cap and stops at the first hit, so a
/// huge tree cannot stall the UI. Symlinks are skipped (as `pesto::walk` does).
fn dir_has_unbacked(
    dir: &Path,
    nzb_status: &HashMap<String, NzbStatusEntry>,
    nzb_disk_index: &HashMap<String, DiskNzbInfo>,
) -> bool {
    const CAP: usize = 50_000;
    let mut stack = vec![dir.to_path_buf()];
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
                let backed = name
                    .to_str()
                    .map(|n| {
                        nzb_status.contains_key(n) || nzb_disk_index.contains_key(&release_key(n))
                    })
                    .unwrap_or(false);
                if !backed {
                    return true;
                }
                if visited >= CAP {
                    return false;
                }
            }
        }
    }
    // No files at all (empty dir) counts as nothing to upload.
    false
}

/// Known media/archive extensions stripped when deriving a release key. Only
/// these are removed (not arbitrary trailing segments), so codec/group tags
/// like `x264` or `-cza` survive and stay part of the match.
const RELEASE_KEY_EXTS: &[&str] = &[
    "mkv", "mp4", "avi", "m2ts", "ts", "mov", "wmv", "flv", "iso", "img", "rar", "zip", "7z",
    "mka", "webm", "m4v", "mpg", "mpeg", "vob",
];

/// Derive a comparison key that matches a media file against an existing `.nzb`.
///
/// The same transformation is applied to both sides so different naming
/// conventions converge:
///   * `Zootopia.2016...DUAL-cza.mkv`            (media file)
///   * `Zootopia.2016...DUAL-cza.nzb`            (NZB named after the release)
///   * `20260427T151003Z_Zootopia...BiOMA.mkv.nzb` (upapasta-generated NZB)
///
/// Steps: strip a trailing `.nzb`, then any trailing known media/archive
/// extensions, then a leading `YYYYMMDDThhmmssZ_` timestamp prefix, and finally
/// keep only lowercase alphanumerics so separators do not affect the match.
pub(crate) fn release_key(name: &str) -> String {
    let mut base = name.to_string();

    // 1. Drop a trailing `.nzb` (case-insensitive).
    if base.len() >= 4 && base[base.len() - 4..].eq_ignore_ascii_case(".nzb") {
        base.truncate(base.len() - 4);
    }

    // 2. Drop trailing known media/archive extensions (e.g. `.mkv.nzb` →
    //    `.mkv` → ``). Loops so doubled extensions are all removed.
    loop {
        let stripped = base
            .rfind('.')
            .map(|dot| {
                let ext = base[dot + 1..].to_ascii_lowercase();
                if RELEASE_KEY_EXTS.contains(&ext.as_str()) {
                    base.truncate(dot);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if !stripped {
            break;
        }
    }

    // 3. Drop a leading timestamp prefix: 8 digits, 'T', 6 digits, 'Z', '_'.
    let b = base.as_bytes();
    if b.len() > 17
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8].eq_ignore_ascii_case(&b'T')
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15].eq_ignore_ascii_case(&b'Z')
        && b[16] == b'_'
    {
        base.drain(..17);
    }

    // 4. Normalize: lowercase alphanumerics only.
    base.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Best-effort byte size of an item: file length, or the recursive sum of a
/// directory's files (capped for very large trees).
fn item_size(path: &Path) -> u64 {
    if path.is_file() {
        return std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    const CAP: usize = 50_000;
    let mut stack = vec![path.to_path_buf()];
    let mut total = 0u64;
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
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
                visited += 1;
                if visited >= CAP {
                    return total;
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::{fmt_bytes, release_key, FileTree};
    use std::fs;

    #[test]
    fn release_key_matches_media_and_nzb_names() {
        // The reported case: a media file and its sibling .nzb (named after the
        // release, no media extension) must produce the same key.
        let media = "Zootopia.2016.1080p.DSNP.WEB-DL.DDP5.1.H.264.DUAL-cza.mkv";
        let nzb = "Zootopia.2016.1080p.DSNP.WEB-DL.DDP5.1.H.264.DUAL-cza.nzb";
        assert_eq!(release_key(media), release_key(nzb));
        assert!(!release_key(media).is_empty());
    }

    #[test]
    fn release_key_handles_upapasta_timestamp_and_double_extension() {
        // upapasta-generated NZBs carry a timestamp prefix and keep the media
        // extension before `.nzb`: `<ts>_<release>.mkv.nzb`.
        let media = "Zootopia.2.2025.1080p.DSNP.WEB-DL.DDP5.1.H.264.DUAL-BiOMA.mkv";
        let nzb =
            "20260427T151003Z_Zootopia.2.2025.1080p.DSNP.WEB-DL.DDP5.1.H.264.DUAL-BiOMA.mkv.nzb";
        assert_eq!(release_key(media), release_key(nzb));
    }

    #[test]
    fn release_key_keeps_codec_and_group_tags() {
        // A trailing alphanumeric tag like `x264` is NOT an extension we strip,
        // so two distinct releases stay distinct.
        assert_ne!(
            release_key("Movie.2024.1080p.x264-AAA.mkv"),
            release_key("Movie.2024.1080p.x264-BBB.mkv")
        );
    }

    #[test]
    fn fmt_bytes_scales_units() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    /// refresh() must stay off the filesystem-walk path: a scan is pending and
    /// the summary is not ready until the background job is applied.
    #[test]
    fn refresh_defers_summary_to_background_scan() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), [0u8; 100]).unwrap();
        fs::write(dir.path().join("b.bin"), [0u8; 50]).unwrap();

        let mut tree = FileTree::new();
        tree.current_dir = dir.path().to_path_buf();
        tree.refresh();

        // Listing is available immediately, but the backed/size numbers are not.
        assert_eq!(tree.items.len(), 2);
        assert!(!tree.summary_ready);

        // Run the deferred job (as the blocking worker would) and fold it back.
        let job = tree.take_scan_job().expect("a scan should be pending");
        let (generation, results) = job.run();
        tree.apply_scan(generation, results);

        assert!(tree.summary_ready);
        // Nothing in the (empty) catalog, so both files are unbacked: 2 items,
        // 2 unbacked, 150 bytes to upload.
        assert_eq!(tree.summary(), (2, 2, 150));
    }

    /// A scan whose generation was superseded (e.g. the user navigated away)
    /// must be discarded rather than overwriting the current directory's state.
    #[test]
    fn stale_scan_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), [0u8; 10]).unwrap();

        let mut tree = FileTree::new();
        tree.current_dir = dir.path().to_path_buf();
        tree.refresh();
        let job = tree.take_scan_job().expect("a scan should be pending");
        let (stale_gen, results) = job.run();

        // Simulate navigating away before the scan returned.
        tree.refresh();
        tree.apply_scan(stale_gen, results);

        // The stale result was dropped, so we are still waiting on the fresh one.
        assert!(!tree.summary_ready);
        assert!(tree.scan_pending);
    }
}
