//! State for the NZB Vault screen.

use std::cmp::Reverse;
use std::path::PathBuf;

use crate::nzb_viewer::{NzbContents, NzbViewerState};

use super::{collect_nzbs_recursive, expand_tilde, App};

/// One entry in the NZB Vault list.
#[derive(Debug, Clone)]
pub struct VaultEntry {
    /// Full path to the `.nzb` file.
    pub path: PathBuf,
    /// Filename (display name).
    pub name: String,
    /// File size in bytes.
    pub file_size: u64,
    /// Last modification time (Unix timestamp).
    pub modified: u64,
    /// Lazily parsed contents (None until the entry is selected).
    pub contents: Option<NzbContents>,
    /// Whether this NZB appears in the catalog.
    pub in_catalog: bool,
    /// Where this NZB came from.
    pub origin: NzbOrigin,
}

/// Where a vault NZB file originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NzbOrigin {
    /// Created by an upapasta upload (`nzb_dir/uploaded/`).
    Uploaded,
    /// Downloaded from a Prowlarr/indexer search (`nzb_dir/downloaded/`).
    Downloaded,
    /// Added manually by the user (root of `nzb_dir`).
    #[default]
    Manual,
}

/// Lightweight metadata about an `.nzb` found on disk, keyed by release key in
/// the browser's disk index. Lets the Browser distinguish a Prowlarr download
/// from a prior upload and flag password-protected releases without consulting
/// the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskNzbInfo {
    /// Origin derived from the immediate parent directory name.
    pub origin: NzbOrigin,
    /// True when the NZB head carries release-password metadata.
    pub has_password: bool,
}

/// Sort order for the NZB Vault list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VaultSort {
    #[default]
    Date,
    Name,
    Size,
}

/// State for the NZB Vault screen (F4).
#[derive(Debug, Default)]
pub struct VaultState {
    pub entries: Vec<VaultEntry>,
    pub selected: usize,
    pub sort: VaultSort,
    /// NZB viewer overlay (Some when open with `v`)
    pub viewer: Option<NzbViewerState>,
    /// Error message if the vault directory could not be read
    pub load_error: Option<String>,
}

impl VaultState {
    /// Toggle the sort mode cycling through Date → Name → Size → Date.
    pub fn cycle_sort(&mut self) {
        self.sort = match self.sort {
            VaultSort::Date => VaultSort::Name,
            VaultSort::Name => VaultSort::Size,
            VaultSort::Size => VaultSort::Date,
        };
        self.apply_sort();
    }

    pub fn apply_sort(&mut self) {
        match self.sort {
            VaultSort::Date => self.entries.sort_by_key(|e| Reverse(e.modified)),
            VaultSort::Name => self.entries.sort_by(|a, b| a.name.cmp(&b.name)),
            VaultSort::Size => self.entries.sort_by_key(|e| Reverse(e.file_size)),
        }
    }

    pub fn selected_entry(&self) -> Option<&VaultEntry> {
        self.entries.get(self.selected)
    }

    #[allow(dead_code)]
    pub fn selected_entry_mut(&mut self) -> Option<&mut VaultEntry> {
        self.entries.get_mut(self.selected)
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if !self.entries.is_empty() && self.selected < self.entries.len() - 1 {
            self.selected += 1;
        }
    }
}

impl App {
    /// Load (or reload) the NZB Vault from the configured nzb_dir.
    ///
    /// Recursively scans all subdirectories. Origin is determined by the
    /// immediate parent folder name: `uploaded/` → Uploaded, `downloaded/` →
    /// Downloaded, anything else (including the root) → Manual.
    pub fn load_vault(&mut self) {
        let nzb_dir = self
            .pesto_config
            .as_ref()
            .and_then(|c| c.nzb_dir.as_deref())
            .map(expand_tilde);

        let Some(dir) = nzb_dir else {
            self.vault.entries.clear();
            self.vault.load_error = Some("nzb_dir not configured in pesto.toml".to_string());
            return;
        };

        if !dir.is_dir() {
            self.vault.entries.clear();
            self.vault.load_error = Some(format!("{}: directory not found", dir.display()));
            return;
        }

        self.vault.load_error = None;

        // Collect catalog NZB paths for cross-reference
        let catalog_paths: std::collections::HashSet<String> = if let Some(ref cat) = self.catalog {
            cat.all_nzb_paths()
                .unwrap_or_default()
                .into_iter()
                .collect()
        } else {
            std::collections::HashSet::new()
        };

        let mut entries: Vec<VaultEntry> = Vec::new();
        collect_nzbs_recursive(&dir, &catalog_paths, &mut entries);

        // Apply current sort
        match self.vault.sort {
            VaultSort::Date => entries.sort_by_key(|e| Reverse(e.modified)),
            VaultSort::Name => entries.sort_by(|a, b| a.name.cmp(&b.name)),
            VaultSort::Size => entries.sort_by_key(|e| Reverse(e.file_size)),
        }

        self.vault.selected = 0;
        self.vault.entries = entries;
        let count = self.vault.entries.len();
        self.status_bar.set(format!(
            "NZB Vault — {} file{}",
            count,
            if count == 1 { "" } else { "s" }
        ));
    }

    /// Parse the selected vault entry (lazy, only when needed).
    pub fn vault_parse_selected(&mut self) {
        let idx = self.vault.selected;
        if let Some(entry) = self.vault.entries.get_mut(idx) {
            if entry.contents.is_none() {
                match crate::nzb_viewer::parse_nzb(&entry.path.to_string_lossy()) {
                    Ok(c) => entry.contents = Some(c),
                    Err(e) => {
                        self.status_bar.set(format!("Parse error: {}", e));
                    }
                }
            }
        }
    }

    /// Open the NZB viewer overlay for the selected vault entry.
    pub fn vault_open_viewer(&mut self) {
        self.vault_parse_selected();
        if let Some(entry) = self.vault.selected_entry() {
            if let Some(ref contents) = entry.contents {
                self.vault.viewer = Some(crate::nzb_viewer::NzbViewerState {
                    contents: contents.clone(),
                    scroll: 0,
                });
            }
        }
    }
}
