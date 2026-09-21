//! Prowlarr integration state.

use crate::prowlarr::{ConnectionStatus, ProwlarrConfig};

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
