//! State for the History screen.

use crate::catalog::{CatalogStats, UploadSummary};
use crate::nzb_viewer::NzbViewerState;

use super::App;

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

impl App {
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
}
