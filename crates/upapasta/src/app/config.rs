//! State for the Config screen and per-session upload overrides.

use pesto::config::ObfuscateMode;

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
