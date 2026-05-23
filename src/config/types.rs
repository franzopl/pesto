use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use parmesan_core::SimdPath;
use serde::Deserialize;
use std::path::PathBuf;

/// Default NNTP-over-TLS port.
pub const DEFAULT_PORT: u16 = 563;
/// Default number of parallel connections.
pub const DEFAULT_CONNECTIONS: usize = 4;
/// Default target size of each article body, in bytes.
pub const DEFAULT_ARTICLE_SIZE: usize = 768_000;
/// Default yEnc line length, in encoded characters.
pub const DEFAULT_LINE_LENGTH: usize = 128;
/// Default number of post attempts per segment before giving up.
pub const DEFAULT_RETRIES: u32 = 3;
/// Default pause between failed post attempts, in seconds.
pub const DEFAULT_RETRY_DELAY: u64 = 1;
/// Default percentage of PAR2 recovery data to generate.
pub const DEFAULT_PAR2: u8 = 10;

/// A fully resolved per-server entry used for failover.
#[derive(Debug, Clone)]
pub struct ServerEntry {
    pub host: String,
    pub port: u16,
    pub ssl: bool,
    pub connections: usize,
    pub username: Option<String>,
    pub password: Option<String>,
    pub retry_delay: u64,
}

/// How much of a post to obfuscate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ObfuscateMode {
    /// No obfuscation: the real file name appears in the subject and the
    /// yEnc header.
    #[default]
    None,
    /// Random subject; the yEnc `name=` field keeps the real file name, so a
    /// standard client still names the download correctly.
    Subject,
    /// Random subject *and* random yEnc `name=` field. Nothing on the wire
    /// reveals the real name — recover it from the `.nzb` or from PAR2 files.
    Full,
}

/// A per-server entry as parsed from `[[servers]]` in the TOML file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileServerEntry {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub ssl: Option<bool>,
    pub connections: Option<usize>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub retry_delay: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub ssl: Option<bool>,
    pub connections: Option<usize>,
    /// Seconds to wait between failed post attempts.
    pub retry_delay: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSection {
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostingSection {
    pub from: Option<String>,
    pub groups: Option<Vec<String>>,
    pub article_size: Option<usize>,
    /// yEnc line length, in encoded characters.
    pub line_length: Option<usize>,
    /// Post attempts per segment before it is recorded as failed.
    pub retries: Option<u32>,
    pub obfuscate: Option<ObfuscateMode>,
    pub par2: Option<u8>,
    /// Confirm each posted article via STAT after posting.
    pub verify: Option<bool>,
    /// Maximum upload rate as a human-readable string, e.g. `"50 MiB/s"`.
    pub upload_rate: Option<String>,
    /// `Date:` header mode: `"now"`, `"random"`, or an RFC 2822 timestamp.
    pub date: Option<String>,
    /// Add `X-No-Archive: yes` to every posted article.
    pub no_archive: Option<bool>,
    /// Fixed domain for `Message-ID` generation. When absent a random domain
    /// is generated per article.
    pub message_id_domain: Option<String>,
    /// Run a deferred STAT check on every posted article after upload finishes.
    pub check: Option<bool>,
    /// Seconds to wait before running the post-check STAT pass. Default: 30.
    pub check_delay: Option<u64>,
    /// Number of STAT attempts per article during post-check. Default: 2.
    pub check_retries: Option<u32>,
    /// Maximum RAM for PAR2 recovery buffers as a human-readable string,
    /// e.g. `"512 MiB"`. When the total buffer size would exceed this limit
    /// the encoder splits recovery blocks into multiple passes, re-reading
    /// the input files once per pass. Default: `"1 GiB"`.
    pub par2_memory_limit: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSection {
    /// Append a record to `history.jsonl` after each upload. Default: true.
    pub history: Option<bool>,
    /// Directory where `history.jsonl` (and `nzb/`) are written.
    pub history_dir: Option<String>,
    /// Default path for the generated `.nzb`. Overridden by `--out`.
    pub nzb: Option<String>,
    /// Directory where `.nzb` files are written by default.
    pub nzb_dir: Option<String>,
    /// Friendly name emitted as `<meta type="name">` in the `.nzb`.
    pub nzb_name: Option<String>,
    /// Extraction password emitted as `<meta type="password">` in the `.nzb`.
    pub nzb_password: Option<String>,
    /// Category emitted as `<meta type="category">` in the `.nzb`.
    pub nzb_category: Option<String>,
    /// Newznab indexer upload configuration.
    #[serde(default)]
    pub indexer: IndexerSection,
    /// Shell command to execute after a successful upload.
    pub post_hook: Option<String>,
    /// Generate a `.nfo` file alongside the `.nzb` after posting.
    pub nfo: Option<bool>,
    /// Resume interrupted uploads from a saved state file. Default: false.
    pub resume: Option<bool>,
    /// Show only a single spinner line instead of the full panel. Default: false.
    pub quiet: Option<bool>,
    /// Ring the terminal bell on completion. Default: false.
    pub bell: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerSection {
    pub url: Option<String>,
    pub api_key: Option<String>,
    pub category: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompressionSection {
    pub format: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifySection {
    pub webhook_url: Option<String>,
    pub ntfy_topic: Option<String>,
}

/// Configuration as parsed from the TOML file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default, rename = "servers")]
    pub extra_servers: Vec<FileServerEntry>,
    #[serde(default)]
    pub posting: PostingSection,
    #[serde(default)]
    pub output: OutputSection,
    #[serde(default)]
    pub compression: CompressionSection,
    #[serde(default)]
    pub notify: NotifySection,
}

/// CLI-supplied overrides.
#[derive(Debug, Default)]
pub struct Overrides {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub ssl: Option<bool>,
    pub connections: Option<usize>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub from: Option<String>,
    pub groups: Option<Vec<String>>,
    pub article_size: Option<usize>,
    pub line_length: Option<usize>,
    pub retries: Option<u32>,
    pub retry_delay: Option<u64>,
    pub obfuscate: Option<ObfuscateMode>,
    pub dry_run: Option<bool>,
    pub par2: Option<u8>,
    pub par2_only: Option<bool>,
    pub par2_memory_limit: Option<u64>,
    pub par2_slice_size: Option<u64>,
    pub par2_slice_count: Option<usize>,
    pub par2_recovery_count: Option<usize>,
    pub threads: Option<usize>,
    pub simd: Option<SimdPath>,
    pub resume: Option<bool>,
    pub verify: Option<bool>,
    pub upload_rate: Option<u64>,
    pub compress_format: Option<String>,
    pub compress_password: Option<String>,
    pub nzb_name: Option<String>,
    pub nzb_password: Option<String>,
    pub nzb_category: Option<String>,
    pub nzb_dir: Option<String>,
    pub no_upload: bool,
    pub history: Option<bool>,
    pub notify: Option<bool>,
    pub date: Option<String>,
    pub no_archive: Option<bool>,
    pub message_id_domain: Option<String>,
    pub post_hook: Option<String>,
    pub no_hooks: bool,
    pub nfo: Option<bool>,
    pub check: Option<bool>,
    pub check_delay_secs: Option<u64>,
    pub check_retries: Option<u32>,
}

/// Fully resolved, validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub ssl: bool,
    pub connections: usize,
    pub username: Option<String>,
    pub password: Option<String>,
    pub retry_delay: u64,
    pub extra_servers: Vec<ServerEntry>,
    pub from: String,
    pub groups: Vec<String>,
    pub article_size: usize,
    pub line_length: usize,
    pub retries: u32,
    pub obfuscate: ObfuscateMode,
    pub date: Option<String>,
    pub no_archive: bool,
    pub message_id_domain: Option<String>,
    pub dry_run: bool,
    pub par2: u8,
    pub par2_memory_limit: Option<usize>,
    pub par2_slice_size: Option<usize>,
    pub par2_slice_count: Option<usize>,
    pub par2_recovery_count: Option<usize>,
    pub par2_only: bool,
    pub threads: usize,
    pub simd: SimdPath,
    pub verify: bool,
    pub resume: bool,
    pub upload_rate: u64,
    pub compress_format: Option<String>,
    pub compress_password: Option<String>,
    pub nzb_name: Option<String>,
    pub nzb_password: Option<String>,
    pub nzb_category: Option<String>,
    pub indexer_url: Option<String>,
    pub indexer_api_key: Option<String>,
    pub indexer_category: Option<String>,
    pub nzb_dir: Option<String>,
    pub no_upload: bool,
    pub history: bool,
    pub history_dir: Option<PathBuf>,
    pub notify_webhook: Option<String>,
    pub notify_ntfy: Option<String>,
    pub notify: Option<bool>,
    pub post_hook: Option<String>,
    pub no_hooks: bool,
    pub nfo: bool,
    pub quiet: bool,
    pub bell: bool,
    pub check: bool,
    pub check_delay_secs: u64,
    pub check_retries: u32,
}

impl Config {
    /// All servers in priority order: primary first, then [`extra_servers`].
    pub fn all_servers(&self) -> impl Iterator<Item = ServerEntry> + '_ {
        std::iter::once(ServerEntry {
            host: self.host.clone(),
            port: self.port,
            ssl: self.ssl,
            connections: self.connections,
            username: self.username.clone(),
            password: self.password.clone(),
            retry_delay: self.retry_delay,
        })
        .chain(self.extra_servers.iter().cloned())
    }

    /// Total number of parallel connections across all servers.
    pub fn total_connections(&self) -> usize {
        self.connections
            + self
                .extra_servers
                .iter()
                .map(|s| s.connections)
                .sum::<usize>()
    }
}

/// Parse a human-readable upload rate string into bytes per second.
pub fn parse_upload_rate(s: &str) -> Result<u64> {
    let s = s.trim();
    let s = s
        .strip_suffix("/s")
        .or_else(|| s.strip_suffix("ps"))
        .unwrap_or(s)
        .trim();

    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_str, unit) = s.split_at(split);
    let value: f64 = num_str
        .trim()
        .parse()
        .with_context(|| format!("invalid upload rate `{}`", s))?;
    let multiplier: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        other => bail!("unknown rate unit `{other}` in `{s}`"),
    };
    Ok((value * multiplier) as u64)
}
