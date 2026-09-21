//! Upload confirmation panel: field list, editing and per-session overrides.

use pesto::config::ObfuscateMode;

use super::config::{ConfirmField, ConfirmFieldView};
use super::{compress_label, obf_label, on_off, App, UNSET};

impl App {
    // ── Upload config panel field editing ─────────────────────────────────────

    /// The fields shown in the panel, in order. `Folder mode` only appears when
    /// a directory is queued (it is a no-op for plain files).
    pub fn confirm_order(&self) -> Vec<ConfirmField> {
        use ConfirmField::*;
        let has_dir = self
            .upload_queue
            .items
            .iter()
            .any(|p| self.queue_info(p).is_dir);
        let mut v = vec![Obfuscate, Par2];
        if has_dir {
            v.push(FolderMode);
        }
        v.extend([
            Compress,
            CompressPassword,
            Check,
            NzbPassword,
            Groups,
            From,
            Category,
            ArticleSize,
        ]);
        v
    }

    /// The field currently under the cursor.
    fn current_confirm_field(&self) -> Option<ConfirmField> {
        self.confirm_order().get(self.confirm_field).copied()
    }

    pub fn confirm_field_next(&mut self) {
        let len = self.confirm_order().len().max(1);
        self.confirm_field = (self.confirm_field + 1) % len;
    }

    pub fn confirm_field_prev(&mut self) {
        let len = self.confirm_order().len().max(1);
        self.confirm_field = if self.confirm_field == 0 {
            len - 1
        } else {
            self.confirm_field - 1
        };
    }

    fn obf_effective(&self) -> ObfuscateMode {
        self.config_state.overrides.obfuscate.unwrap_or(
            self.pesto_config
                .as_ref()
                .map(|c| c.obfuscate)
                .unwrap_or(ObfuscateMode::None),
        )
    }

    fn par2_effective(&self) -> u8 {
        self.config_state
            .overrides
            .par2
            .unwrap_or(self.pesto_config.as_ref().map(|c| c.par2).unwrap_or(10))
    }

    fn compress_effective(&self) -> String {
        self.config_state
            .overrides
            .compress_format
            .clone()
            .or_else(|| {
                self.pesto_config
                    .as_ref()
                    .and_then(|c| c.compress_format.clone())
            })
            .unwrap_or_else(|| "none".to_string())
    }

    /// Cycle / toggle enum and bool fields; for text/number fields, enter edit mode.
    pub fn confirm_field_activate(&mut self) {
        let Some(field) = self.current_confirm_field() else {
            return;
        };
        match field {
            ConfirmField::Obfuscate => self.confirm_cycle_obfuscate(true),
            ConfirmField::FolderMode => {
                let cur = self.effective_folder_mode();
                self.config_state.overrides.folder_mode = Some(cur.next());
            }
            ConfirmField::Compress => self.confirm_cycle_compress(true),
            ConfirmField::Check => self.confirm_toggle_check(),
            // Number / text fields → enter edit mode prefilled with the current value.
            ConfirmField::Par2 => self.confirm_start_edit(self.par2_effective().to_string()),
            ConfirmField::ArticleSize => {
                let kb = self.config_state.overrides.article_size_kb.unwrap_or(
                    self.pesto_config
                        .as_ref()
                        .map(|c| c.article_size / 1024)
                        .unwrap_or(768),
                );
                self.confirm_start_edit(kb.to_string());
            }
            ConfirmField::CompressPassword => {
                let cur = self
                    .config_state
                    .overrides
                    .compress_password
                    .clone()
                    .or_else(|| {
                        self.pesto_config
                            .as_ref()
                            .and_then(|c| c.compress_password.clone())
                    })
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
            ConfirmField::NzbPassword => {
                let cur = self
                    .config_state
                    .overrides
                    .nzb_password
                    .clone()
                    .or_else(|| {
                        self.pesto_config
                            .as_ref()
                            .and_then(|c| c.nzb_password.clone())
                    })
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
            ConfirmField::Groups => {
                let cur = self
                    .config_state
                    .overrides
                    .groups
                    .clone()
                    .or_else(|| self.pesto_config.as_ref().map(|c| c.groups.join(", ")))
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
            ConfirmField::From => {
                let cur = self
                    .config_state
                    .overrides
                    .from
                    .clone()
                    .or_else(|| self.pesto_config.as_ref().map(|c| c.from.clone()))
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
            ConfirmField::Category => {
                let cur = self
                    .config_state
                    .overrides
                    .nzb_category
                    .clone()
                    .or_else(|| {
                        self.pesto_config
                            .as_ref()
                            .and_then(|c| c.nzb_category.clone())
                    })
                    .unwrap_or_default();
                self.confirm_start_edit(cur);
            }
        }
    }

    fn confirm_start_edit(&mut self, prefill: String) {
        self.confirm_edit_buf = prefill;
        self.confirm_editing = true;
    }

    fn confirm_cycle_obfuscate(&mut self, _forward: bool) {
        let next = match self.obf_effective() {
            ObfuscateMode::None => ObfuscateMode::Full,
            ObfuscateMode::Full => ObfuscateMode::FullShared,
            ObfuscateMode::FullShared => ObfuscateMode::Light,
            ObfuscateMode::Light => ObfuscateMode::None,
            ObfuscateMode::Article => ObfuscateMode::None,
        };
        self.config_state.overrides.obfuscate = Some(next);
    }

    fn confirm_cycle_compress(&mut self, _forward: bool) {
        let next = match self.compress_effective().as_str() {
            "none" => "zip",
            "zip" => "7z",
            "7z" => "rar",
            _ => "none",
        };
        self.config_state.overrides.compress_format = Some(next.to_string());
    }

    fn confirm_toggle_check(&mut self) {
        let cur = self
            .config_state
            .overrides
            .check
            .unwrap_or(self.pesto_config.as_ref().map(|c| c.check).unwrap_or(false));
        self.config_state.overrides.check = Some(!cur);
    }

    /// `→` / `l` / Space: advance cycle/number/toggle fields in place.
    pub fn confirm_field_increment(&mut self) {
        match self.current_confirm_field() {
            Some(ConfirmField::Obfuscate) => self.confirm_cycle_obfuscate(true),
            Some(ConfirmField::Compress) => self.confirm_cycle_compress(true),
            Some(ConfirmField::Check) => self.confirm_toggle_check(),
            Some(ConfirmField::FolderMode) => {
                let cur = self.effective_folder_mode();
                self.config_state.overrides.folder_mode = Some(cur.next());
            }
            Some(ConfirmField::Par2) => {
                let cur = self.par2_effective();
                self.config_state.overrides.par2 = Some(if cur >= 50 { 0 } else { cur + 5 });
            }
            _ => {}
        }
    }

    /// `←` / `h`: step cycle/number/toggle fields backwards.
    pub fn confirm_field_decrement(&mut self) {
        match self.current_confirm_field() {
            // Cycles are short; stepping backwards is the same as cycling forward.
            Some(ConfirmField::Obfuscate) => self.confirm_cycle_obfuscate(false),
            Some(ConfirmField::Compress) => self.confirm_cycle_compress(false),
            Some(ConfirmField::Check) => self.confirm_toggle_check(),
            Some(ConfirmField::FolderMode) => {
                let cur = self.effective_folder_mode();
                self.config_state.overrides.folder_mode = Some(cur.next());
            }
            Some(ConfirmField::Par2) => {
                let cur = self.par2_effective();
                self.config_state.overrides.par2 =
                    Some(if cur == 0 { 50 } else { cur.saturating_sub(5) });
            }
            _ => {}
        }
    }

    /// Commit the text edit buffer into the relevant session override.
    pub fn confirm_confirm_edit(&mut self) {
        let buf = self.confirm_edit_buf.trim().to_string();
        let set_opt = |b: String| if b.is_empty() { None } else { Some(b) };
        match self.current_confirm_field() {
            Some(ConfirmField::Par2) => {
                self.config_state.overrides.par2 = buf.parse::<u8>().ok().map(|v| v.min(50));
            }
            Some(ConfirmField::ArticleSize) => {
                self.config_state.overrides.article_size_kb =
                    buf.parse::<usize>().ok().filter(|kb| *kb > 0);
            }
            Some(ConfirmField::CompressPassword) => {
                self.config_state.overrides.compress_password = set_opt(buf);
            }
            Some(ConfirmField::NzbPassword) => {
                self.config_state.overrides.nzb_password = set_opt(buf);
            }
            Some(ConfirmField::Groups) => {
                self.config_state.overrides.groups = set_opt(buf);
            }
            Some(ConfirmField::From) => {
                self.config_state.overrides.from = set_opt(buf);
            }
            Some(ConfirmField::Category) => {
                self.config_state.overrides.nzb_category = set_opt(buf);
            }
            _ => {}
        }
        self.confirm_editing = false;
        self.confirm_edit_buf.clear();
    }

    /// Rendered snapshot of all panel fields, in display order.
    pub fn confirm_field_views(&self) -> Vec<ConfirmFieldView> {
        let ov = &self.config_state.overrides;
        let cfg = self.pesto_config.as_ref();
        let mask = |raw: &str| -> String {
            if raw.is_empty() {
                UNSET.to_string()
            } else if self.confirm_show_password {
                raw.to_string()
            } else {
                "•".repeat(raw.len().min(20))
            }
        };
        self.confirm_order()
            .into_iter()
            .map(|field| {
                let (label, value, hint): (&'static str, String, &'static str) = match field {
                    ConfirmField::Obfuscate => (
                        "Obfuscate",
                        obf_label(self.obf_effective()).to_string(),
                        "←→ cycle",
                    ),
                    ConfirmField::Par2 => (
                        "PAR2 %",
                        format!("{}%", self.par2_effective()),
                        "←→ or Enter",
                    ),
                    ConfirmField::FolderMode => (
                        "Folder",
                        self.effective_folder_mode().label().to_string(),
                        "←→ cycle",
                    ),
                    ConfirmField::Compress => (
                        "Compress",
                        compress_label(&self.compress_effective()),
                        "←→ cycle",
                    ),
                    ConfirmField::CompressPassword => {
                        let raw = ov
                            .compress_password
                            .clone()
                            .or_else(|| cfg.and_then(|c| c.compress_password.clone()))
                            .unwrap_or_default();
                        ("Zip pass", mask(&raw), "Enter edit  Tab show")
                    }
                    ConfirmField::Check => (
                        "Check",
                        on_off(ov.check.unwrap_or(cfg.map(|c| c.check).unwrap_or(false)))
                            .to_string(),
                        "←→ toggle: streaming STAT check during upload",
                    ),
                    ConfirmField::NzbPassword => {
                        let raw = ov
                            .nzb_password
                            .clone()
                            .or_else(|| cfg.and_then(|c| c.nzb_password.clone()))
                            .unwrap_or_default();
                        ("NZB pass", mask(&raw), "Enter edit  Tab show")
                    }
                    ConfirmField::Groups => (
                        "Groups",
                        ov.groups
                            .clone()
                            .or_else(|| cfg.map(|c| c.groups.join(", ")))
                            .unwrap_or_else(|| UNSET.to_string()),
                        "Enter edit",
                    ),
                    ConfirmField::From => (
                        "From",
                        ov.from
                            .clone()
                            .or_else(|| cfg.map(|c| c.from.clone()))
                            .unwrap_or_else(|| UNSET.to_string()),
                        "Enter edit",
                    ),
                    ConfirmField::Category => (
                        "Category",
                        ov.nzb_category
                            .clone()
                            .or_else(|| cfg.and_then(|c| c.nzb_category.clone()))
                            .unwrap_or_else(|| UNSET.to_string()),
                        "Enter edit",
                    ),
                    ConfirmField::ArticleSize => {
                        let kb = ov
                            .article_size_kb
                            .unwrap_or(cfg.map(|c| c.article_size / 1024).unwrap_or(768));
                        ("Article", format!("{kb} KB"), "Enter edit")
                    }
                };
                ConfirmFieldView { label, value, hint }
            })
            .collect()
    }

    /// One-line explanation of the current obfuscation mode, for the panel.
    pub fn obfuscate_legend(&self) -> &'static str {
        match self.obf_effective() {
            ObfuscateMode::None => "None: public subject + real filenames",
            ObfuscateMode::Full => "Full: random subject + poster + filenames",
            ObfuscateMode::FullShared => {
                "Full (shared): one random name for the whole release, so indexers can still group it"
            }
            ObfuscateMode::Light => {
                "Light: like Full (shared), but subject and yEnc name match exactly"
            }
            ObfuscateMode::Article => {
                "Article: unique subject + yEnc name + poster per article (experimental)"
            }
        }
    }

    pub fn confirm_cancel_edit(&mut self) {
        self.confirm_editing = false;
        self.confirm_edit_buf.clear();
    }

    pub fn confirm_toggle_password_reveal(&mut self) {
        self.confirm_show_password = !self.confirm_show_password;
    }

    /// Reset all confirm-panel overrides and close the panel.
    pub fn confirm_close(&mut self) {
        self.show_upload_confirm = false;
        self.confirm_editing = false;
        self.confirm_edit_buf.clear();
    }
}
