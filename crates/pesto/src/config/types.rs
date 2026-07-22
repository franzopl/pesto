use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use parmesan::SimdPath;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default NNTP-over-TLS port.
pub const DEFAULT_PORT: u16 = 563;
/// Default number of parallel connections.
pub const DEFAULT_CONNECTIONS: usize = 4;
/// Default keepalive interval in seconds. Send `MODE READER` on idle connections
/// every this many seconds to prevent the server from closing them silently.
/// Set to 0 to disable.
pub const DEFAULT_KEEPALIVE_SECS: u64 = 60;
/// Default target size of each article body, in bytes.
pub const DEFAULT_ARTICLE_SIZE: usize = 768_000;
/// Default yEnc line length, in encoded characters.
pub const DEFAULT_LINE_LENGTH: usize = 128;
/// Default number of post attempts per segment before giving up.
pub const DEFAULT_RETRIES: u32 = 3;
/// Default pause between failed post attempts, in seconds.
pub const DEFAULT_RETRY_DELAY: u64 = 1;
/// Default per-command read timeout on an NNTP connection, in seconds.
///
/// This bounds how long a worker waits for a server response before treating
/// the socket as dead. It must be generous enough never to fire on a slow but
/// healthy upload (a large article on a slow link can legitimately take tens of
/// seconds to acknowledge), while still rescuing the process from a silently
/// dropped TCP connection long before the OS keepalive would (~2 h on Linux,
/// ~4.5 min on Windows). 120 s is a deliberately conservative middle ground.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
// 1 = one sequential POST per connection (RFC 3977-compliant). Throughput comes
// from parallel connections, not intra-connection pipelining. Depth > 1 pipelines
// POST commands without waiting for the server's 340, which violates RFC 3977 and
// is rejected by strict servers (e.g. Newshosting returns 441 on pipelined POSTs).
pub const DEFAULT_PIPELINE_DEPTH: usize = 1;
/// Maximum depth the adaptive pipeline will auto-select.
pub const MAX_AUTO_PIPELINE_DEPTH: usize = 8;
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
    /// Per-command read timeout, in seconds. See [`DEFAULT_TIMEOUT_SECS`].
    pub timeout: u64,
}

/// What to do when the NZB user-destination already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum NzbConflict {
    /// Overwrite (hardlink/copy) the existing file silently. Default.
    #[default]
    Overwrite,
    /// Rename the destination by appending `-1`, `-2`, … until the name is free.
    Rename,
    /// Abort the NZB write and print an error. The archive copy is still kept.
    Fail,
}

/// Whether to obfuscate a post.
///
/// When enabled, both the subject line and the yEnc `name=` field are
/// randomised on the wire. The real filename is always preserved in the
/// generated NZB so that download clients can restore it correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ObfuscateMode {
    /// No obfuscation: the real file name appears in the subject and yEnc header.
    #[default]
    None,
    /// Randomise subject and yEnc `name=` on the wire; the NZB always carries
    /// the real filename so download clients work without PAR2 recovery.
    Full,
    /// Like `full` but each individual article gets a unique subject and From
    /// header, making segment grouping by wire metadata impossible.
    /// Experimental — requires the NZB to download.
    #[value(hide = true)]
    Paranoid,
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
    /// Per-command read timeout, in seconds. See [`DEFAULT_TIMEOUT_SECS`].
    pub timeout: Option<u64>,
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
    /// Per-command read timeout, in seconds. See [`DEFAULT_TIMEOUT_SECS`].
    pub timeout: Option<u64>,
    /// Keepalive interval in seconds. A `MODE READER` command is sent on idle
    /// connections every this many seconds to prevent the server from closing
    /// them silently during long PAR2 computations or check-phase waits.
    /// Set to 0 to disable. See [`DEFAULT_KEEPALIVE_SECS`].
    pub keepalive: Option<u64>,
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
    /// Maximum upload rate as a human-readable string, e.g. `"50 MiB/s"`.
    pub upload_rate: Option<String>,
    /// `Date:` header mode: `"now"`, `"random"`, or an RFC 2822 timestamp.
    pub date: Option<String>,
    /// Add `X-No-Archive: yes` to every posted article.
    pub no_archive: Option<bool>,
    /// Fixed domain for `Message-ID` generation. When absent a random domain
    /// is generated per article.
    pub message_id_domain: Option<String>,
    /// Confirm every posted article via a streaming STAT check that runs
    /// concurrently with the upload (each article is checked a few seconds
    /// after it posts; misses are reposted automatically). Default: true.
    pub check: Option<bool>,
    /// Seconds to wait after an article posts before its first STAT check.
    /// Default: 5.
    pub check_delay: Option<u64>,
    /// Number of STAT attempts per posted copy before triggering a repost.
    /// Default: 3.
    pub check_retries: Option<u32>,
    /// Number of dedicated parallel NNTP connections for the streaming check
    /// queue. Default: 0 (a small pool sized `min(4, connections)`).
    pub check_connections: Option<usize>,
    /// Number of times to re-post an article the check queue still can't
    /// find. Mirrors nyuu's `check-post-tries`. Default: 1.
    pub check_post_retries: Option<u32>,
    /// Publish the NZB (and run post-upload hooks) even when some articles
    /// are still confirmed missing on the server after every
    /// `check_post_retries` round. Default: false — pesto refuses to write
    /// an NZB that references content it never confirmed is retrievable.
    pub allow_incomplete_nzb: Option<bool>,
    /// Number of articles to send per connection before reading responses.
    /// Values > 1 enable NNTP pipelining, which cuts per-article RTT cost.
    /// Default: 1.
    pub pipeline_depth: Option<usize>,
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
    /// Save a per-upload DEBUG log under `<history_dir>/logs/` for later
    /// analysis, regardless of `-v`. Default: true. Disable with
    /// `--no-session-log`.
    pub session_log: Option<bool>,
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
    /// Tags emitted as multiple `<meta type="tag">` elements in the `.nzb`.
    #[serde(default)]
    pub nzb_tags: Vec<String>,
    /// Prowlarr connection settings (URL + API key for search/download).
    #[serde(default)]
    pub indexer: IndexerSection,
    /// Shell command to execute before the upload begins. Non-zero exit aborts.
    /// Kept for backward compatibility; prefer `pre_hooks`.
    pub pre_hook: Option<String>,
    /// Shell commands to execute before the upload begins (one per entry).
    #[serde(default)]
    pub pre_hooks: Vec<String>,
    /// Shell command to execute after a successful upload.
    /// Kept for backward compatibility; prefer `post_hooks`.
    pub post_hook: Option<String>,
    /// Shell commands to execute after a successful upload (one per entry).
    #[serde(default)]
    pub post_hooks: Vec<String>,
    /// Skip the executable scripts in `~/.config/pesto/hooks/` and
    /// `~/.config/pesto/pre-hooks/`. The `post_hooks` and `pre_hooks` config
    /// values are unaffected — only the directory scan is suppressed.
    /// Default: false. Also settable via `--no-hooks`.
    pub no_hooks: Option<bool>,
    /// Generate a `.nfo` file alongside the `.nzb` after posting.
    pub nfo: Option<bool>,
    /// How to handle a conflict when the user-destination `.nzb` already exists.
    /// `"overwrite"` (default), `"rename"` (append `-1`, `-2`, …), `"fail"`.
    pub nzb_conflict: Option<NzbConflict>,
    /// Resume interrupted uploads from a saved state file. Default: false.
    pub resume: Option<bool>,
    /// Show only a single spinner line instead of the full panel. Default: false.
    pub quiet: Option<bool>,
    /// Ring the terminal bell on completion. Default: false.
    pub bell: Option<bool>,
}

/// Prowlarr connection settings stored under `[output.indexer]` in the TOML.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerSection {
    pub url: Option<String>,
    pub api_key: Option<String>,
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
    pub upload_rate: Option<u64>,
    pub compress_format: Option<String>,
    pub compress_password: Option<String>,
    pub nzb_name: Option<String>,
    pub nzb_password: Option<String>,
    pub nzb_category: Option<String>,
    pub nzb_tags: Vec<String>,
    /// Raw `--tmdb` value, e.g. `movie/12345` or `tv:12345`; parsed and
    /// validated in [`Config::resolve`].
    pub tmdb: Option<String>,
    /// Raw `--imdb-id` value, e.g. `tt1234567`; parsed and validated in
    /// [`Config::resolve`].
    pub imdb_id: Option<String>,
    /// Raw `--tvdb-id` value, e.g. `81189`; parsed and validated in
    /// [`Config::resolve`].
    pub tvdb_id: Option<String>,
    /// Raw `--mal-id` value, e.g. `1535`; parsed and validated in
    /// [`Config::resolve`].
    pub mal_id: Option<String>,
    pub nzb_dir: Option<String>,
    pub history: Option<bool>,
    pub notify: Option<bool>,
    pub date: Option<String>,
    pub no_archive: Option<bool>,
    pub message_id_domain: Option<String>,
    pub pre_hooks: Vec<String>,
    pub post_hooks: Vec<String>,
    pub no_hooks: Option<bool>,
    pub nfo: Option<bool>,
    pub nzb_conflict: Option<NzbConflict>,
    pub check: Option<bool>,
    pub check_delay_secs: Option<u64>,
    pub check_retries: Option<u32>,
    pub check_connections: Option<usize>,
    pub check_post_retries: Option<u32>,
    pub allow_incomplete_nzb: Option<bool>,
    pub pipeline_depth: Option<usize>,
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
    /// Per-command read timeout, in seconds. See [`DEFAULT_TIMEOUT_SECS`].
    pub timeout: u64,
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
    pub resume: bool,
    pub upload_rate: u64,
    pub compress_format: Option<String>,
    pub compress_password: Option<String>,
    pub nzb_name: Option<String>,
    pub nzb_password: Option<String>,
    pub nzb_category: Option<String>,
    pub nzb_tags: Vec<String>,
    /// TMDb reference emitted as `<meta type="tmdbid">`, formatted as
    /// `movie/<id>` or `tv/<id>`. See [`crate::nzb::parse_tmdb_ref`].
    pub tmdb_id: Option<String>,
    /// Media kind of `tmdb_id`, kept alongside it to derive a default
    /// `nzb_category` when the user hasn't set one explicitly.
    pub tmdb_kind: Option<crate::nzb::TmdbKind>,
    /// IMDb ID emitted as `<meta type="imdbid">`, e.g. `tt1234567`.
    pub imdb_id: Option<String>,
    /// TheTVDB ID emitted as `<meta type="tvdbid">`.
    pub tvdb_id: Option<String>,
    /// MyAnimeList ID emitted as `<meta type="malid">`.
    pub mal_id: Option<String>,
    pub indexer_url: Option<String>,
    pub indexer_api_key: Option<String>,
    pub nzb_dir: Option<String>,
    pub history: bool,
    pub history_dir: Option<PathBuf>,
    pub notify_webhook: Option<String>,
    pub notify_ntfy: Option<String>,
    pub notify: Option<bool>,
    pub pre_hooks: Vec<String>,
    pub post_hooks: Vec<String>,
    pub no_hooks: bool,
    pub nfo: bool,
    pub nzb_conflict: NzbConflict,
    pub quiet: bool,
    pub bell: bool,
    pub check: bool,
    pub check_delay_secs: u64,
    pub check_retries: u32,
    pub check_connections: usize,
    pub check_post_retries: u32,
    pub allow_incomplete_nzb: bool,
    pub pipeline_depth: usize,
    /// Keepalive interval in seconds; 0 = disabled. See [`DEFAULT_KEEPALIVE_SECS`].
    pub keepalive_interval: u64,
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
            timeout: self.timeout,
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

    /// Desired number of dedicated connections for the streaming check
    /// queue, before the caller bounds it against the configured total
    /// (`post_files_with_progress_and_cancel` carves this out of
    /// `total_connections()` rather than opening it on top — see there for
    /// why). `0` resolves to a small pool sized as a *fraction* of the
    /// total (roughly 8%, capped at 4) rather than a flat number — a flat
    /// cap of 4 is a sensible ~8% at a real-world `connections=50` (checked
    /// against production traffic: STAT's cost is small enough relative to
    /// POST that 4 dedicated connections comfortably keep up with 46
    /// posting connections), but at a low total like `connections=4` a flat
    /// 4 would try to reserve the *entire* pool for checking and leave
    /// nothing for uploading. Scaling with the total avoids that: checking
    /// gets at least 1 connection once there's more than one to spare, and
    /// tops out at 4 once the total is large enough that 4 is already a
    /// small slice of it.
    pub fn effective_check_connections(&self) -> usize {
        if self.check_connections == 0 {
            let total = self.total_connections();
            if total < 2 {
                0
            } else {
                (total / 12).clamp(1, 4)
            }
        } else {
            self.check_connections
        }
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
