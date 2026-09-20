//! Queue entry metadata: how a queued path becomes an NZB.

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
