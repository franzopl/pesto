//! State for the "run a hook on the selected release" overlay.

use std::path::PathBuf;

/// State of the "run a hook on the selected release" overlay. Lists the
/// executable hooks in `~/.config/pesto/hooks/` so the user runs exactly one,
/// not all of them. Carries the resolved selection so the choice survives even
/// if the Browser cursor moves.
#[derive(Debug, Clone)]
pub struct HookPickerState {
    /// Release name derived from the selected item (for status + NZB lookup).
    pub release_name: String,
    /// A directly selected `.nzb` (Vault entry / `.nzb` file); otherwise the
    /// NZB is resolved from `nzb_dir` by release key when the hook runs.
    pub direct_nzb: Option<PathBuf>,
    /// The selected media file/folder on disk, when the selection is media (not
    /// an `.nzb`). Used to generate a `.nfo` via mediainfo when none exists
    /// next to the `.nzb`, so hooks receive `PESTO_NFO`.
    pub media_path: Option<PathBuf>,
    /// Executable hook scripts available to run.
    pub hooks: Vec<PathBuf>,
    /// Index of the highlighted hook.
    pub selected: usize,
    /// Last successful run time per hook *name* for this release, from the
    /// catalog. Drives the "✓ sent <date>" marker and the re-send confirmation.
    pub runs: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    /// Index awaiting a re-send confirmation (a hook already sent for this
    /// release). The next Enter on the same row runs it; navigating clears it.
    pub pending_confirm: Option<usize>,
}

impl HookPickerState {
    pub fn new(
        release_name: String,
        direct_nzb: Option<PathBuf>,
        media_path: Option<PathBuf>,
        hooks: Vec<PathBuf>,
        runs: std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        Self {
            release_name,
            direct_nzb,
            media_path,
            hooks,
            selected: 0,
            runs,
            pending_confirm: None,
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.pending_confirm = None;
    }

    pub fn move_down(&mut self) {
        if !self.hooks.is_empty() && self.selected < self.hooks.len() - 1 {
            self.selected += 1;
        }
        self.pending_confirm = None;
    }

    pub fn selected_hook(&self) -> Option<&PathBuf> {
        self.hooks.get(self.selected)
    }

    /// The last-sent time for a hook path, if this release was sent through it.
    pub fn sent_at(&self, hook: &std::path::Path) -> Option<chrono::DateTime<chrono::Utc>> {
        let name = hook.file_name()?.to_str()?;
        self.runs.get(name).copied()
    }
}
