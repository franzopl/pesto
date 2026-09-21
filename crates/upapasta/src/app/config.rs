//! State for the Config screen and per-session upload overrides.

use pesto::config::{Config as PestoConfig, ObfuscateMode};

use super::{apply_indexer_field, upload_prefs_path, App};

/// Per-session upload overrides set via the Config screen.
/// None = use the value from the loaded pesto config (or built-in default).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionOverrides {
    pub from: Option<String>,
    /// Comma-separated newsgroup list.
    pub groups: Option<String>,
    pub obfuscate: Option<ObfuscateMode>,
    /// 0–50 %
    pub par2: Option<u8>,
    pub article_size_kb: Option<usize>,
    pub check: Option<bool>,
    pub nzb_password: Option<String>,
    pub nzb_category: Option<String>,
    pub compress_password: Option<String>,
    /// Compression archive format: `none`, `zip`, `7z` or `rar`.
    pub compress_format: Option<String>,
    /// How a queued directory becomes NZB(s). `None` = the default (`single`).
    pub folder_mode: Option<FolderMode>,
}

/// How a queued directory is turned into NZB(s) at upload time.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FolderMode {
    /// One NZB for the whole folder (a single release). The default.
    #[default]
    Single,
    /// One NZB per file inside the folder (no combined release NZB).
    PerFile,
    /// One NZB per file *and* a combined "season" NZB over all of them.
    Season,
}

impl FolderMode {
    pub fn label(self) -> &'static str {
        match self {
            FolderMode::Single => "single NZB",
            FolderMode::PerFile => "per-file",
            FolderMode::Season => "season (per-file + combined)",
        }
    }

    pub fn next(self) -> Self {
        match self {
            FolderMode::Single => FolderMode::PerFile,
            FolderMode::PerFile => FolderMode::Season,
            FolderMode::Season => FolderMode::Single,
        }
    }
}

/// One editable setting in the upload-config panel. The order here is the order
/// shown on screen and navigated with j/k; both the render and the key handlers
/// derive from this single list, so there are no fragile parallel indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmField {
    Obfuscate,
    Par2,
    FolderMode,
    Compress,
    CompressPassword,
    Check,
    NzbPassword,
    Groups,
    From,
    Category,
    ArticleSize,
}

/// A rendered snapshot of one field for the panel.
pub struct ConfirmFieldView {
    pub label: &'static str,
    pub value: String,
    pub hint: &'static str,
}

/// State for the Config screen.
#[derive(Debug, Default)]
pub struct ConfigState {
    /// Index of the selected field in the field list.
    pub selected: usize,
    /// Whether we are currently editing the selected field.
    pub editing: bool,
    /// Scratch buffer for text input.
    pub edit_buf: String,
    /// Per-session overrides the user has set.
    pub overrides: SessionOverrides,
}

impl App {
    /// Total number of editable fields in the Config screen.
    pub const CONFIG_FIELD_COUNT: usize = 12;

    pub fn config_select_next(&mut self) {
        self.config_state.selected =
            (self.config_state.selected + 1).min(Self::CONFIG_FIELD_COUNT - 1);
    }

    pub fn config_select_prev(&mut self) {
        self.config_state.selected = self.config_state.selected.saturating_sub(1);
    }

    /// Enter edit mode for the currently selected field.
    pub fn config_start_edit(&mut self) {
        let ov = &self.config_state.overrides;
        let cfg = self.pesto_config.as_ref();
        let buf = match self.config_state.selected {
            0 => ov
                .from
                .clone()
                .or_else(|| cfg.map(|c| c.from.clone()))
                .unwrap_or_default(),
            1 => ov
                .groups
                .clone()
                .or_else(|| cfg.map(|c| c.groups.join(",")))
                .unwrap_or_default(),
            2 => {
                // obfuscate: cycle on confirm, no text buf needed
                self.config_cycle_obfuscate();
                return;
            }
            3 => ov
                .par2
                .map(|v| v.to_string())
                .or_else(|| cfg.map(|c| c.par2.to_string()))
                .unwrap_or_else(|| "10".to_string()),
            4 => ov
                .article_size_kb
                .map(|v| v.to_string())
                .or_else(|| cfg.map(|c| (c.article_size / 1024).to_string()))
                .unwrap_or_else(|| "750".to_string()),
            5 => {
                // check: cycle bool
                self.config_cycle_check();
                return;
            }
            6 => ov
                .nzb_password
                .clone()
                .or_else(|| cfg.and_then(|c| c.nzb_password.clone()))
                .unwrap_or_default(),
            7 => ov
                .nzb_category
                .clone()
                .or_else(|| cfg.and_then(|c| c.nzb_category.clone()))
                .unwrap_or_default(),
            8 => ov
                .compress_password
                .clone()
                .or_else(|| cfg.and_then(|c| c.compress_password.clone()))
                .unwrap_or_default(),
            // 9 = separator "── Prowlarr ──" (not editable)
            9 => return,
            10 => self
                .prowlarr
                .url_override
                .clone()
                .or_else(|| cfg?.indexer_url.clone())
                .unwrap_or_default(),
            11 => self
                .prowlarr
                .api_key_override
                .clone()
                .or_else(|| cfg?.indexer_api_key.clone())
                .unwrap_or_default(),
            _ => return,
        };
        self.config_state.edit_buf = buf;
        self.config_state.editing = true;
    }

    /// Commit the edit buffer to the current field override.
    pub fn config_confirm_edit(&mut self) {
        let buf = self.config_state.edit_buf.trim().to_string();
        let ov = &mut self.config_state.overrides;
        match self.config_state.selected {
            0 => ov.from = if buf.is_empty() { None } else { Some(buf) },
            1 => ov.groups = if buf.is_empty() { None } else { Some(buf) },
            3 => {
                ov.par2 = buf.parse::<u8>().ok().map(|v| v.min(50));
            }
            4 => {
                ov.article_size_kb = buf.parse::<usize>().ok();
            }
            6 => ov.nzb_password = if buf.is_empty() { None } else { Some(buf) },
            7 => ov.nzb_category = if buf.is_empty() { None } else { Some(buf) },
            8 => ov.compress_password = if buf.is_empty() { None } else { Some(buf) },
            10 => {
                let val = if buf.is_empty() { None } else { Some(buf) };
                self.prowlarr.url_override = val.clone();
                // Reset connection status so user can re-test with new URL
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
                // Prowlarr config is persisted to config.toml so it survives
                // restarts (unlike the upload overrides, which are session-only).
                self.persist_indexer_field("url", val.as_deref());
                self.config_state.editing = false;
                self.config_state.edit_buf.clear();
                return;
            }
            11 => {
                let val = if buf.is_empty() { None } else { Some(buf) };
                self.prowlarr.api_key_override = val.clone();
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
                self.persist_indexer_field("api_key", val.as_deref());
                self.config_state.editing = false;
                self.config_state.edit_buf.clear();
                return;
            }
            _ => {}
        }
        self.config_state.editing = false;
        self.config_state.edit_buf.clear();
        self.status_bar.set("Override saved (session only)");
    }

    /// Persist a `[output.indexer]` field (e.g. `url`, `api_key`) to the pesto
    /// `config.toml` so Prowlarr settings survive a restart. Uses `toml_edit` to
    /// preserve the rest of the file (comments, formatting, ordering). A `None`
    /// value removes the key. Also updates the in-memory resolved config so the
    /// change takes effect immediately even after the session override is reset.
    fn persist_indexer_field(&mut self, field: &str, value: Option<&str>) {
        // Mirror the change into the already-resolved in-memory config.
        if let Some(cfg) = self.pesto_config.as_mut() {
            let owned = value.map(str::to_string);
            match field {
                "url" => cfg.indexer_url = owned,
                "api_key" => cfg.indexer_api_key = owned,
                _ => {}
            }
        }

        let Some(path) = self
            .config_path
            .clone()
            .or_else(pesto::config::default_config_path)
        else {
            self.status_bar
                .set("Saved for this session (could not locate config.toml)");
            return;
        };

        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let new_text = match apply_indexer_field(&text, field, value) {
            Ok(t) => t,
            Err(e) => {
                self.status_bar.set(format!(
                    "Saved for this session (config.toml parse error: {e})"
                ));
                return;
            }
        };

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, new_text) {
            Ok(()) => self
                .status_bar
                .set(format!("Saved Prowlarr {field} to {}", path.display())),
            Err(e) => self
                .status_bar
                .set(format!("Saved for this session (write failed: {e})")),
        }
    }

    pub fn config_cancel_edit(&mut self) {
        self.config_state.editing = false;
        self.config_state.edit_buf.clear();
    }

    /// Reset the selected field override to None (use config default).
    pub fn config_reset_field(&mut self) {
        let ov = &mut self.config_state.overrides;
        match self.config_state.selected {
            0 => ov.from = None,
            1 => ov.groups = None,
            2 => ov.obfuscate = None,
            3 => ov.par2 = None,
            4 => ov.article_size_kb = None,
            5 => ov.check = None,
            6 => ov.nzb_password = None,
            7 => ov.nzb_category = None,
            8 => ov.compress_password = None,
            // 9 = separator "── Prowlarr ──" (not selectable/resettable)
            10 => {
                self.prowlarr.url_override = None;
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
            }
            11 => {
                self.prowlarr.api_key_override = None;
                self.prowlarr.status = crate::prowlarr::ConnectionStatus::Unknown;
            }
            _ => {}
        }
        self.status_bar.set("Field reset to config default");
    }

    /// Reset all overrides.
    pub fn config_reset_all(&mut self) {
        self.config_state.overrides = SessionOverrides::default();
        self.status_bar.set("All overrides cleared");
    }

    fn config_cycle_obfuscate(&mut self) {
        use ObfuscateMode::*;
        let cfg_default = self
            .pesto_config
            .as_ref()
            .map(|c| c.obfuscate)
            .unwrap_or(ObfuscateMode::None);
        let current = self.config_state.overrides.obfuscate.unwrap_or(cfg_default);
        self.config_state.overrides.obfuscate = Some(match current {
            None => Full,
            Full => FullShared,
            FullShared => Light,
            Light => None,
            Article => None,
        });
        self.status_bar.set("Obfuscate mode changed");
    }

    fn config_cycle_check(&mut self) {
        let cfg_default = self.pesto_config.as_ref().map(|c| c.check).unwrap_or(true);
        let current = self.config_state.overrides.check.unwrap_or(cfg_default);
        self.config_state.overrides.check = Some(!current);
        self.status_bar.set("Check mode toggled");
    }

    /// Apply session overrides on top of the effective config, returning a
    /// modified clone ready for upload.
    pub fn effective_config_with_overrides(&self) -> Option<PestoConfig> {
        let mut cfg = self.pesto_config.clone()?;
        let ov = &self.config_state.overrides;
        if let Some(ref from) = ov.from {
            cfg.from = from.clone();
        }
        if let Some(ref groups_str) = ov.groups {
            cfg.groups = groups_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(obf) = ov.obfuscate {
            cfg.obfuscate = obf;
        }
        if let Some(par2) = ov.par2 {
            cfg.par2 = par2;
        }
        if let Some(kb) = ov.article_size_kb {
            cfg.article_size = kb * 1024;
        }
        if let Some(check) = ov.check {
            cfg.check = check;
        }
        if let Some(ref pw) = ov.nzb_password {
            cfg.nzb_password = Some(pw.clone());
        }
        if let Some(ref cat) = ov.nzb_category {
            cfg.nzb_category = Some(cat.clone());
        }
        if let Some(ref pw) = ov.compress_password {
            cfg.compress_password = Some(pw.clone());
        }
        if let Some(ref fmt) = ov.compress_format {
            cfg.compress_format = if fmt == "none" {
                None
            } else {
                Some(fmt.clone())
            };
        }
        Some(cfg)
    }

    /// The effective folder mode for this batch (override or the default).
    pub fn effective_folder_mode(&self) -> FolderMode {
        self.config_state.overrides.folder_mode.unwrap_or_default()
    }
}

impl App {
    /// Persist current session overrides to disk so they are pre-filled next time.
    pub fn save_upload_prefs(&self) {
        if let Some(path) = upload_prefs_path() {
            if let Ok(json) = serde_json::to_string_pretty(&self.config_state.overrides) {
                let _ = std::fs::write(path, json);
            }
        }
    }

    /// Load previously saved session overrides and merge them into config_state.
    /// Values already set (e.g. from the pesto config) are not overwritten.
    pub fn load_upload_prefs(&mut self) {
        let Some(path) = upload_prefs_path() else {
            return;
        };
        let Ok(data) = std::fs::read_to_string(path) else {
            return;
        };
        if let Ok(prefs) = serde_json::from_str::<SessionOverrides>(&data) {
            let o = &mut self.config_state.overrides;
            if o.obfuscate.is_none() {
                o.obfuscate = prefs.obfuscate;
            }
            if o.par2.is_none() {
                o.par2 = prefs.par2;
            }
            if o.check.is_none() {
                o.check = prefs.check;
            }
            if o.nzb_password.is_none() {
                o.nzb_password = prefs.nzb_password;
            }
            if o.groups.is_none() {
                o.groups = prefs.groups;
            }
            if o.compress_password.is_none() {
                o.compress_password = prefs.compress_password;
            }
            if o.compress_format.is_none() {
                o.compress_format = prefs.compress_format;
            }
            if o.from.is_none() {
                o.from = prefs.from;
            }
            if o.nzb_category.is_none() {
                o.nzb_category = prefs.nzb_category;
            }
            if o.article_size_kb.is_none() {
                o.article_size_kb = prefs.article_size_kb;
            }
            // folder_mode is intentionally NOT restored: it is a per-batch choice
            // (defaults to a single release NZB each session) so an old "season"
            // selection can never silently change how a folder uploads later.
        }
    }
}
