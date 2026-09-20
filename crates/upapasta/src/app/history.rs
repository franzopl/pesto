//! State for the History screen.

use crate::catalog::{CatalogStats, UploadSummary};
use crate::nzb_viewer::NzbViewerState;

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
