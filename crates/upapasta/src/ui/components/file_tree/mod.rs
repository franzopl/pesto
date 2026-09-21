use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::app::{DiskNzbInfo, NzbOrigin};
use crate::catalog::NzbStatusEntry;

use scan::{item_size, path_is_backed};

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
    /// Release keys (see [`release_key`]) of releases already sent through a
    /// hook (e.g. an indexer upload). Drives the "sent" marker in the list.
    pub hooked: HashSet<String>,
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
            hooked: HashSet::new(),
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
    /// Replace the set of release keys already sent through a hook. Only affects
    /// the row marker (not backed status), so no scan invalidation is needed.
    pub fn set_hooked_index(&mut self, keys: HashSet<String>) {
        self.hooked = keys;
    }

    /// Whether `name`'s release has already been sent through a hook.
    pub fn is_hooked(&self, name: &str) -> bool {
        !self.hooked.is_empty() && self.hooked.contains(&release_key(name))
    }

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
}

mod render;
mod scan;
#[cfg(test)]
mod tests;

pub(crate) use scan::release_key;
