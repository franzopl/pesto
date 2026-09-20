//! Queue entry metadata: how a queued path becomes an NZB.

use std::path::PathBuf;

use super::App;

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

impl App {
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
}
