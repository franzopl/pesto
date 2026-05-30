use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

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
    /// Names of files currently being uploaded (basename).
    pub uploading: HashSet<String>,
    /// First visible item index — managed manually to get correct scroll behaviour.
    scroll_offset: usize,
    /// Number of items that fit in the last rendered area; updated at render time.
    visible_height: usize,
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
            uploading: HashSet::new(),
            scroll_offset: 0,
            visible_height: 20,
        };
        tree.refresh();
        tree
    }

    /// Replace the NZB status map (called after catalog refresh). The summary
    /// and the unbacked filter depend on it, so recompute both.
    pub fn set_nzb_status(&mut self, status: HashMap<String, NzbStatusEntry>) {
        self.nzb_status = status;
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
            self.recompute_summary();
            self.apply_filter();
        }
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
    fn recompute_summary(&mut self) {
        let total = self.all_items.len();
        let mut unbacked = 0usize;
        let mut bytes = 0u64;
        for p in &self.all_items {
            if !self.is_backed(p) {
                unbacked += 1;
                bytes += item_size(p);
            }
        }
        self.summary = (total, unbacked, bytes);
    }

    /// `(total items, unbacked items, bytes still to upload)` for the status line.
    pub fn summary(&self) -> (usize, usize, u64) {
        self.summary
    }

    /// Whether a path is already backed up (has an NZB in the catalog).
    ///
    /// A file is backed when it is in the catalog by full path or base name.
    /// A directory is backed when it was uploaded as a release (its folder name
    /// is in the catalog) *or* every file under it is individually backed —
    /// i.e. it is unbacked if any child still needs uploading.
    fn is_backed(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let full = path.to_string_lossy();
        if self.nzb_status.contains_key(full.as_ref()) || self.nzb_status.contains_key(name) {
            return true;
        }
        if path.is_dir() {
            return !dir_has_unbacked(path, &self.nzb_status);
        }
        false
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
        let summary = if unbacked > 0 {
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

/// Whether `dir` contains at least one file (recursively) that is not in the
/// catalog by its base name. Walks with a cap and stops at the first hit, so a
/// huge tree cannot stall the UI. Symlinks are skipped (as `pesto::walk` does).
fn dir_has_unbacked(dir: &Path, nzb_status: &HashMap<String, NzbStatusEntry>) -> bool {
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
                let in_catalog = name
                    .to_str()
                    .map(|n| nzb_status.contains_key(n))
                    .unwrap_or(false);
                if !in_catalog {
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
    use super::fmt_bytes;

    #[test]
    fn fmt_bytes_scales_units() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1024), "1.0 KB");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
