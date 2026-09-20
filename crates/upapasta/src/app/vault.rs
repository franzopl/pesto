//! State for the NZB Vault screen.

use std::cmp::Reverse;
use std::path::PathBuf;

use crate::nzb_viewer::{NzbContents, NzbViewerState};

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
