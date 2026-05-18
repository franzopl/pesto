//! Configuration loading and resolution.
//!
//! Settings come from an optional TOML file; any field may be overridden by a
//! CLI flag. [`FileConfig`] mirrors the TOML layout (everything optional);
//! [`Config`] is the fully resolved, validated result used by the rest of the
//! program.

use crate::article::random_from;
use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::Deserialize;
use std::path::{Path, PathBuf};

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
///
/// The primary server's fields live directly on [`Config`] for backward
/// compatibility. Additional failover servers are stored in
/// [`Config::extra_servers`].
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

/// Parse a human-readable upload rate string into bytes per second.
///
/// Accepted formats: `"50 MiB/s"`, `"10 MB/s"`, `"1024 KiB/s"`,
/// `"100 KB/s"`, `"500"` (bare number = bytes/sec).
/// Unit matching is case-insensitive.
pub fn parse_upload_rate(s: &str) -> Result<u64> {
    let s = s.trim();
    // strip optional trailing "/s" or "ps"
    let s = s
        .strip_suffix("/s")
        .or_else(|| s.strip_suffix("ps"))
        .unwrap_or(s)
        .trim();

    // split at first non-digit, non-dot character
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

/// Path of the config file `pesto` loads when `--config` is not given.
///
/// Returns the pesto config directory (the parent folder of `config.toml`).
/// Same logic as [`default_config_path`] without the filename component.
pub fn config_dir() -> Option<PathBuf> {
    default_config_path().and_then(|p| p.parent().map(PathBuf::from))
}

/// On Unix: follows the XDG Base Directory spec (`$XDG_CONFIG_HOME/pesto/config.toml`),
/// falling back to `$HOME/.config/pesto/config.toml`.
/// On Windows: uses `%APPDATA%\pesto\config.toml`.
/// Returns `None` only when the relevant environment variable is not set.
pub fn default_config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("pesto").join("config.toml"))
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(xdg).join("pesto").join("config.toml"));
        }
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("pesto")
                .join("config.toml")
        })
    }
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
///
/// Credentials live here (no separate `[auth]` section per failover server).
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

/// Configuration as parsed from the TOML file. Every field is optional so the
/// file may be partial and the remainder supplied via CLI flags.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub auth: AuthSection,
    /// Optional array of failover servers (`[[servers]]`). When non-empty,
    /// the first entry becomes the primary server; `[server]` / `[auth]` are
    /// ignored.
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSection {
    /// Write a record to `~/.config/upapasta/history.jsonl` after each upload.
    /// Default: true.
    pub history: Option<bool>,
    /// Default path for the generated `.nzb`. Overridden by `--out`.
    pub nzb: Option<String>,
    /// Directory where `.nzb` files are written by default. The filename is
    /// derived from the upload name. Overridden by `--out` or `output.nzb`.
    pub nzb_dir: Option<String>,
    /// Friendly name emitted as `<meta type="name">` in the `.nzb`.
    pub nzb_name: Option<String>,
    /// Extraction password emitted as `<meta type="password">` in the `.nzb`.
    /// Defaults to the archive password when `--password` is set.
    pub nzb_password: Option<String>,
    /// Category emitted as `<meta type="category">` in the `.nzb`.
    pub nzb_category: Option<String>,
    /// Newznab indexer upload configuration.
    #[serde(default)]
    pub indexer: IndexerSection,
    /// Shell command to execute after a successful upload. Receives upload
    /// details via environment variables (`PESTO_NZB`, `PESTO_NFO`, …).
    pub post_hook: Option<String>,
    /// Generate a `.nfo` file alongside the `.nzb` after posting.
    /// Default: false.
    pub nfo: Option<bool>,
    /// Resume interrupted uploads from a saved state file. Default: false.
    pub resume: Option<bool>,
    /// Show only a single spinner line instead of the full panel. Default: false.
    pub quiet: Option<bool>,
    /// Ring the terminal bell on completion. Default: false.
    pub bell: Option<bool>,
}

/// Newznab API configuration for automatic NZB upload after posting.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexerSection {
    /// Base URL of the indexer (e.g. `https://my.indexer.example`).
    pub url: Option<String>,
    /// Newznab API key.
    pub api_key: Option<String>,
    /// Category ID or name to assign the upload.
    pub category: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompressionSection {
    /// Archive format: `"7z"` (default), `"zip"`, or `"rar"`.
    pub format: Option<String>,
}

/// `[notify]` config section for completion notifications.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifySection {
    /// HTTP webhook URL (Discord, Slack, Telegram, or generic).
    pub webhook_url: Option<String>,
    /// ntfy.sh topic name or full topic URL.
    pub ntfy_topic: Option<String>,
}

impl FileConfig {
    /// Load and parse a TOML config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file `{}`", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config file `{}`", path.display()))
    }
}

/// CLI-supplied overrides. A `Some` value wins over the file; `None` defers to
/// the file (or the built-in default).
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
    /// `false` disables resume (equivalent to `--no-resume`).
    pub resume: Option<bool>,
    /// `false` disables post-verification via `STAT` (equivalent to `--no-verify`).
    pub verify: Option<bool>,
    /// Maximum upload rate in bytes/sec; `0` means unlimited.
    pub upload_rate: Option<u64>,
    /// Archive format to use when compressing (`"7z"`, `"zip"`, `"rar"`).
    /// `None` means no compression unless `compress_password` is set.
    pub compress_format: Option<String>,
    /// Password for the archive, already resolved (random generation done by caller).
    pub compress_password: Option<String>,
    /// Friendly name for the `.nzb` `<meta type="name">` element.
    pub nzb_name: Option<String>,
    /// Explicit `.nzb` password meta. Falls back to `compress_password`.
    pub nzb_password: Option<String>,
    /// `.nzb` category meta.
    pub nzb_category: Option<String>,
    /// Directory where `.nzb` files are saved by default.
    pub nzb_dir: Option<String>,
    /// When true, skip the indexer NZB upload for this run.
    pub no_upload: bool,
    /// Override history writing: `Some(false)` = `--no-history`.
    pub history: Option<bool>,
    /// `--no-notify`: suppress notifications for this run.
    pub notify: Option<bool>,
    /// `Date:` header mode: `"now"`, `"random"`, or a fixed RFC 2822 string.
    pub date: Option<String>,
    /// Add `X-No-Archive: yes` to every posted article.
    pub no_archive: Option<bool>,
    /// Fixed domain for `Message-ID`. `None` = random per article.
    pub message_id_domain: Option<String>,
    /// Shell command to run after a successful upload.
    pub post_hook: Option<String>,
    /// Generate a `.nfo` file next to the `.nzb` after posting.
    pub nfo: Option<bool>,
}

/// Fully resolved, validated configuration.
#[derive(Debug, Clone)]
pub struct Config {
    // Primary server (index 0 of the server pool).
    pub host: String,
    pub port: u16,
    pub ssl: bool,
    pub connections: usize,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Seconds to wait between failed post attempts (primary server).
    pub retry_delay: u64,
    /// Failover servers. Workers rotate into these when the primary fails.
    /// The primary's fields above are always tried first.
    pub extra_servers: Vec<ServerEntry>,
    pub from: String,
    pub groups: Vec<String>,
    pub article_size: usize,
    /// yEnc line length, in encoded characters.
    pub line_length: usize,
    /// Post attempts per segment before it is recorded as failed.
    pub retries: u32,
    /// How much of each post to obfuscate.
    pub obfuscate: ObfuscateMode,
    /// `Date:` header mode: `"now"`, `"random"`, or an RFC 2822 timestamp.
    /// `None` means omit the header (server fills it in).
    pub date: Option<String>,
    /// Add `X-No-Archive: yes` to every posted article.
    pub no_archive: bool,
    /// Fixed domain for `Message-ID`. `None` = random per article.
    pub message_id_domain: Option<String>,
    /// If true, skip the network and just simulate posting.
    pub dry_run: bool,
    /// Percentage of PAR2 recovery data to generate (0 to disable).
    pub par2: u8,
    /// Only generate PAR2 files without uploading them.
    pub par2_only: bool,
    /// Confirm each posted article with `STAT` and repost on failure.
    pub verify: bool,
    /// Load and save a resume state file to skip already-posted segments.
    pub resume: bool,
    /// Maximum upload rate in bytes/sec across all connections; 0 = unlimited.
    pub upload_rate: u64,
    /// Archive format to use before posting. `None` = no compression.
    pub compress_format: Option<String>,
    /// Password for the archive. `None` = no password.
    /// Set by `--password`; stored in the `.nzb` `<meta type="password">`.
    pub compress_password: Option<String>,
    /// Friendly name for `<meta type="name">` in the `.nzb`.
    pub nzb_name: Option<String>,
    /// Explicit password for `<meta type="password">` in the `.nzb`.
    /// Falls back to `compress_password` when absent.
    pub nzb_password: Option<String>,
    /// Category for `<meta type="category">` in the `.nzb`.
    pub nzb_category: Option<String>,
    /// Newznab indexer to upload the `.nzb` to after posting.
    pub indexer_url: Option<String>,
    pub indexer_api_key: Option<String>,
    pub indexer_category: Option<String>,
    /// Default directory for `.nzb` output. The filename is derived from the
    /// upload name. Overridden by `--out` or `output.nzb`.
    pub nzb_dir: Option<String>,
    /// Skip the indexer upload for this run.
    pub no_upload: bool,
    /// Append a record to the shared history catalog after each upload.
    pub history: bool,
    /// Webhook URL for completion notifications.
    pub notify_webhook: Option<String>,
    /// ntfy.sh topic name or URL for completion notifications.
    pub notify_ntfy: Option<String>,
    /// When `Some(false)`, skip notifications for this run (`--no-notify`).
    /// When `Some(true)`, force notifications even if no URL is configured
    /// in the file config (rarely useful; `--notify` does nothing without a URL).
    pub notify: Option<bool>,
    /// Shell command to run after a successful upload.
    pub post_hook: Option<String>,
    /// Generate a `.nfo` file next to the `.nzb` after posting.
    pub nfo: bool,
    /// Show only a single spinner line (quiet mode).
    pub quiet: bool,
    /// Ring the terminal bell on completion.
    pub bell: bool,
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

impl Config {
    /// Resolve a [`Config`] from an optional file config plus CLI overrides.
    ///
    /// Precedence, highest first: CLI override, file value, built-in default.
    /// Returns an error if a required field (host, from, groups) is missing.
    pub fn resolve(file: FileConfig, cli: Overrides) -> Result<Self> {
        let dry_run = cli.dry_run.unwrap_or(false);
        let par2_only = cli.par2_only.unwrap_or(false);

        // If [[servers]] is present, it takes precedence over [server]/[auth].
        let (host, port, ssl, connections, username, password, retry_delay, extra_servers) =
            if !file.extra_servers.is_empty() {
                let mut iter = file.extra_servers.into_iter();
                let primary = iter.next().unwrap();
                let host = cli
                    .host
                    .or(primary.host)
                    .context("first [[servers]] entry has no `host`")?;
                let port = cli.port.or(primary.port).unwrap_or(DEFAULT_PORT);
                let ssl = cli.ssl.or(primary.ssl).unwrap_or(true);
                let connections = cli
                    .connections
                    .or(primary.connections)
                    .unwrap_or(DEFAULT_CONNECTIONS);
                let username = cli.username.or(primary.username);
                let password = cli.password.or(primary.password);
                let retry_delay = cli
                    .retry_delay
                    .or(primary.retry_delay)
                    .unwrap_or(DEFAULT_RETRY_DELAY);
                let extras: Vec<ServerEntry> = iter
                    .map(|e| -> Result<ServerEntry> {
                        Ok(ServerEntry {
                            host: e.host.context("[[servers]] entry missing `host`")?,
                            port: e.port.unwrap_or(DEFAULT_PORT),
                            ssl: e.ssl.unwrap_or(true),
                            connections: e.connections.unwrap_or(DEFAULT_CONNECTIONS),
                            username: e.username,
                            password: e.password,
                            retry_delay: e.retry_delay.unwrap_or(DEFAULT_RETRY_DELAY),
                        })
                    })
                    .collect::<Result<_>>()?;
                (
                    host,
                    port,
                    ssl,
                    connections,
                    username,
                    password,
                    retry_delay,
                    extras,
                )
            } else {
                let host = if dry_run || par2_only {
                    cli.host
                        .or(file.server.host)
                        .unwrap_or_else(|| "localhost".into())
                } else {
                    cli.host
                        .or(file.server.host)
                        .context("no `host` set: provide [server].host or --host")?
                };
                (
                    host,
                    cli.port.or(file.server.port).unwrap_or(DEFAULT_PORT),
                    cli.ssl.or(file.server.ssl).unwrap_or(true),
                    cli.connections
                        .or(file.server.connections)
                        .unwrap_or(DEFAULT_CONNECTIONS),
                    cli.username.or(file.auth.username),
                    cli.password.or(file.auth.password),
                    cli.retry_delay
                        .or(file.server.retry_delay)
                        .unwrap_or(DEFAULT_RETRY_DELAY),
                    vec![],
                )
            };

        // A `from` is never required: when the user pins neither a config
        // value nor `--from`, post under a freshly generated random identity.
        let from = cli.from.or(file.posting.from).unwrap_or_else(random_from);

        let groups = if par2_only {
            cli.groups
                .or(file.posting.groups)
                .unwrap_or_else(|| vec!["none".into()])
        } else {
            cli.groups
                .or(file.posting.groups)
                .filter(|g| !g.is_empty())
                .context("no `groups` set: provide [posting].groups or --groups")?
        };

        Ok(Config {
            host,
            port,
            ssl,
            connections,
            username,
            password,
            retry_delay,
            extra_servers,
            from,
            groups,
            article_size: cli
                .article_size
                .or(file.posting.article_size)
                .unwrap_or(DEFAULT_ARTICLE_SIZE),
            line_length: cli
                .line_length
                .or(file.posting.line_length)
                .unwrap_or(DEFAULT_LINE_LENGTH),
            retries: cli
                .retries
                .or(file.posting.retries)
                .unwrap_or(DEFAULT_RETRIES)
                .max(1),
            obfuscate: cli.obfuscate.or(file.posting.obfuscate).unwrap_or_default(),
            dry_run,
            par2: cli.par2.or(file.posting.par2).unwrap_or(DEFAULT_PAR2),
            par2_only,
            verify: cli.verify.or(file.posting.verify).unwrap_or(false),
            resume: cli
                .resume
                .unwrap_or_else(|| file.output.resume.unwrap_or(false)),
            upload_rate: {
                // CLI `--rate` wins; fall back to config file string.
                if let Some(rate) = cli.upload_rate {
                    rate
                } else if let Some(s) = file.posting.upload_rate {
                    parse_upload_rate(&s)?
                } else {
                    0
                }
            },
            compress_format: cli.compress_format.or(file.compression.format),
            compress_password: cli.compress_password,
            nzb_name: cli.nzb_name.or(file.output.nzb_name),
            nzb_password: cli.nzb_password.or(file.output.nzb_password),
            nzb_category: cli.nzb_category.or(file.output.nzb_category),
            nzb_dir: cli.nzb_dir.or(file.output.nzb_dir),
            indexer_url: file.output.indexer.url,
            indexer_api_key: file.output.indexer.api_key,
            indexer_category: file.output.indexer.category,
            no_upload: cli.no_upload,
            history: cli
                .history
                .unwrap_or_else(|| file.output.history.unwrap_or(true)),
            notify_webhook: file.notify.webhook_url,
            notify_ntfy: file.notify.ntfy_topic,
            notify: cli.notify,
            date: cli.date.or(file.posting.date),
            no_archive: cli.no_archive.or(file.posting.no_archive).unwrap_or(false),
            message_id_domain: cli.message_id_domain.or(file.posting.message_id_domain),
            post_hook: cli.post_hook.or(file.output.post_hook),
            nfo: cli.nfo.unwrap_or_else(|| file.output.nfo.unwrap_or(false)),
            quiet: file.output.quiet.unwrap_or(false),
            bell: file.output.bell.unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_overrides_win_over_file() {
        let mut file = FileConfig::default();
        file.server.host = Some("file-host".into());
        file.server.port = Some(119);
        file.posting.from = Some("file <f@x>".into());
        file.posting.groups = Some(vec!["a.b.file".into()]);

        let cli = Overrides {
            host: Some("cli-host".into()),
            ..Default::default()
        };

        let cfg = Config::resolve(file, cli).unwrap();
        assert_eq!(cfg.host, "cli-host");
        assert_eq!(cfg.port, 119);
    }

    #[test]
    fn defaults_apply_when_unset() {
        let mut file = FileConfig::default();
        file.server.host = Some("h".into());
        file.posting.from = Some("f <f@x>".into());
        file.posting.groups = Some(vec!["a.b.c".into()]);

        let cfg = Config::resolve(file, Overrides::default()).unwrap();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.connections, DEFAULT_CONNECTIONS);
        assert!(cfg.ssl);
    }

    #[test]
    fn missing_required_field_errors() {
        let cfg = Config::resolve(FileConfig::default(), Overrides::default());
        assert!(cfg.is_err());
    }

    #[test]
    fn obfuscate_mode_parses_from_toml_and_defaults_to_none() {
        let file: FileConfig = toml::from_str("[posting]\nobfuscate = \"full\"\n").unwrap();
        assert_eq!(file.posting.obfuscate, Some(ObfuscateMode::Full));
        assert_eq!(ObfuscateMode::default(), ObfuscateMode::None);
    }

    // ── parse_upload_rate ─────────────────────────────────────────────────────

    #[test]
    fn parse_rate_bare_bytes() {
        assert_eq!(parse_upload_rate("1024").unwrap(), 1024);
    }

    #[test]
    fn parse_rate_kib() {
        assert_eq!(parse_upload_rate("10 KiB/s").unwrap(), 10 * 1024);
    }

    #[test]
    fn parse_rate_mib() {
        assert_eq!(parse_upload_rate("50 MiB/s").unwrap(), 50 * 1024 * 1024);
    }

    #[test]
    fn parse_rate_mb_case_insensitive() {
        assert_eq!(parse_upload_rate("2 MB/s").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn parse_rate_gib() {
        assert_eq!(parse_upload_rate("1 GiB/s").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_rate_unknown_unit_errors() {
        assert!(parse_upload_rate("10 TiB/s").is_err());
    }

    #[test]
    fn parse_rate_not_a_number_errors() {
        assert!(parse_upload_rate("fast").is_err());
    }

    // ── [[servers]] array ─────────────────────────────────────────────────────

    fn base_overrides() -> Overrides {
        Overrides {
            groups: Some(vec!["alt.test".into()]),
            ..Default::default()
        }
    }

    #[test]
    fn single_servers_entry_becomes_primary() {
        let file: FileConfig = toml::from_str(
            r#"
            [[servers]]
            host = "news.example.com"
            port = 119
            ssl = false
            connections = 8
            "#,
        )
        .unwrap();

        let cfg = Config::resolve(file, base_overrides()).unwrap();
        assert_eq!(cfg.host, "news.example.com");
        assert_eq!(cfg.port, 119);
        assert!(!cfg.ssl);
        assert_eq!(cfg.connections, 8);
        assert!(cfg.extra_servers.is_empty());
    }

    #[test]
    fn multiple_servers_first_is_primary_rest_are_extra() {
        let file: FileConfig = toml::from_str(
            r#"
            [[servers]]
            host = "primary.example.com"
            [[servers]]
            host = "backup.example.com"
            connections = 2
            "#,
        )
        .unwrap();

        let cfg = Config::resolve(file, base_overrides()).unwrap();
        assert_eq!(cfg.host, "primary.example.com");
        assert_eq!(cfg.extra_servers.len(), 1);
        assert_eq!(cfg.extra_servers[0].host, "backup.example.com");
        assert_eq!(cfg.extra_servers[0].connections, 2);
    }

    #[test]
    fn servers_entry_missing_host_errors() {
        let file: FileConfig = toml::from_str(
            r#"
            [[servers]]
            port = 119
            "#,
        )
        .unwrap();

        assert!(Config::resolve(file, base_overrides()).is_err());
    }

    #[test]
    fn extra_server_missing_host_errors() {
        let file: FileConfig = toml::from_str(
            r#"
            [[servers]]
            host = "primary.example.com"
            [[servers]]
            port = 119
            "#,
        )
        .unwrap();

        assert!(Config::resolve(file, base_overrides()).is_err());
    }

    #[test]
    fn total_connections_sums_all_servers() {
        let file: FileConfig = toml::from_str(
            r#"
            [[servers]]
            host = "a.example.com"
            connections = 4
            [[servers]]
            host = "b.example.com"
            connections = 2
            "#,
        )
        .unwrap();

        let cfg = Config::resolve(file, base_overrides()).unwrap();
        assert_eq!(cfg.total_connections(), 6);
    }

    // ── error messages are actionable ────────────────────────────────────────

    #[test]
    fn missing_host_error_mentions_host() {
        let mut file = FileConfig::default();
        file.posting.groups = Some(vec!["alt.test".into()]);
        let err = Config::resolve(file, Overrides::default()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("host"), "expected 'host' in error: {msg}");
    }

    #[test]
    fn missing_groups_error_mentions_groups() {
        let mut file = FileConfig::default();
        file.server.host = Some("h".into());
        let err = Config::resolve(file, Overrides::default()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("groups"), "expected 'groups' in error: {msg}");
    }

    #[test]
    fn extra_server_missing_host_error_is_actionable() {
        let file: FileConfig =
            toml::from_str("[[servers]]\nhost = \"primary\"\n[[servers]]\nport = 119\n").unwrap();
        let err = Config::resolve(file, base_overrides()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("host"), "expected 'host' in error: {msg}");
    }

    // ── all defaults ──────────────────────────────────────────────────────────

    fn minimal_file() -> FileConfig {
        let mut f = FileConfig::default();
        f.server.host = Some("h".into());
        f.posting.groups = Some(vec!["alt.test".into()]);
        f
    }

    #[test]
    fn all_numeric_defaults_match_constants() {
        let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert_eq!(cfg.connections, DEFAULT_CONNECTIONS);
        assert_eq!(cfg.article_size, DEFAULT_ARTICLE_SIZE);
        assert_eq!(cfg.line_length, DEFAULT_LINE_LENGTH);
        assert_eq!(cfg.retries, DEFAULT_RETRIES);
        assert_eq!(cfg.retry_delay, DEFAULT_RETRY_DELAY);
        assert_eq!(cfg.par2, DEFAULT_PAR2);
    }

    #[test]
    fn all_boolean_defaults_are_correct() {
        let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
        assert!(cfg.ssl, "ssl should default to true");
        assert!(!cfg.dry_run);
        assert!(!cfg.par2_only);
        assert!(!cfg.verify);
        assert!(!cfg.resume);
        assert!(!cfg.no_archive);
        assert!(cfg.history, "history should default to true");
        assert!(!cfg.nfo);
    }

    #[test]
    fn optional_string_fields_default_to_none() {
        let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
        assert!(cfg.username.is_none());
        assert!(cfg.password.is_none());
        assert!(cfg.compress_format.is_none());
        assert!(cfg.compress_password.is_none());
        assert!(cfg.nzb_name.is_none());
        assert!(cfg.nzb_password.is_none());
        assert!(cfg.nzb_category.is_none());
        assert!(cfg.nzb_dir.is_none());
        assert!(cfg.date.is_none());
        assert!(cfg.message_id_domain.is_none());
        assert!(cfg.post_hook.is_none());
        assert!(cfg.notify_webhook.is_none());
        assert!(cfg.notify_ntfy.is_none());
        assert!(cfg.notify.is_none());
        assert_eq!(cfg.upload_rate, 0);
    }

    #[test]
    fn from_is_generated_randomly_when_not_set() {
        // Without a pinned `from`, each resolve produces a different identity.
        let a = Config::resolve(minimal_file(), Overrides::default())
            .unwrap()
            .from;
        let b = Config::resolve(minimal_file(), Overrides::default())
            .unwrap()
            .from;
        assert_ne!(a, b, "random from should differ between calls");
        assert!(a.contains('@'), "from should be address-shaped");
    }

    #[test]
    fn retries_zero_is_clamped_to_one() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                retries: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.retries, 1);
    }

    // ── TOML round-trips ──────────────────────────────────────────────────────

    #[test]
    fn toml_server_section_is_parsed() {
        let file: FileConfig = toml::from_str(
            r#"
            [server]
            host = "news.example.com"
            port = 119
            ssl = false
            connections = 8
            retry_delay = 5
            "#,
        )
        .unwrap();
        let cfg = Config::resolve(
            file,
            Overrides {
                groups: Some(vec!["alt.test".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.host, "news.example.com");
        assert_eq!(cfg.port, 119);
        assert!(!cfg.ssl);
        assert_eq!(cfg.connections, 8);
        assert_eq!(cfg.retry_delay, 5);
    }

    #[test]
    fn toml_auth_section_sets_credentials() {
        let file: FileConfig = toml::from_str(
            r#"
            [server]
            host = "h"
            [auth]
            username = "alice"
            password = "s3cr3t"
            [posting]
            groups = ["alt.test"]
            "#,
        )
        .unwrap();
        let cfg = Config::resolve(file, Overrides::default()).unwrap();
        assert_eq!(cfg.username.as_deref(), Some("alice"));
        assert_eq!(cfg.password.as_deref(), Some("s3cr3t"));
    }

    #[test]
    fn toml_posting_section_sets_all_fields() {
        let file: FileConfig = toml::from_str(
            r#"
            [server]
            host = "h"
            [posting]
            groups = ["alt.test"]
            from = "Bot <bot@example.com>"
            article_size = 500000
            line_length = 64
            retries = 5
            par2 = 20
            verify = true
            obfuscate = "subject"
            date = "now"
            no_archive = true
            message_id_domain = "example.com"
            upload_rate = "10 MiB/s"
            "#,
        )
        .unwrap();
        let cfg = Config::resolve(file, Overrides::default()).unwrap();
        assert_eq!(cfg.from, "Bot <bot@example.com>");
        assert_eq!(cfg.article_size, 500_000);
        assert_eq!(cfg.line_length, 64);
        assert_eq!(cfg.retries, 5);
        assert_eq!(cfg.par2, 20);
        assert!(cfg.verify);
        assert_eq!(cfg.obfuscate, ObfuscateMode::Subject);
        assert_eq!(cfg.date.as_deref(), Some("now"));
        assert!(cfg.no_archive);
        assert_eq!(cfg.message_id_domain.as_deref(), Some("example.com"));
        assert_eq!(cfg.upload_rate, 10 * 1024 * 1024);
    }

    #[test]
    fn toml_output_section_sets_fields() {
        let file: FileConfig = toml::from_str(
            r#"
            [server]
            host = "h"
            [posting]
            groups = ["alt.test"]
            [output]
            nzb_name = "My Release"
            nzb_category = "TV > HD"
            nzb_dir = "/tmp/nzb"
            history = false
            resume = true
            post_hook = "notify.sh"
            nfo = true
            "#,
        )
        .unwrap();
        let cfg = Config::resolve(file, Overrides::default()).unwrap();
        assert_eq!(cfg.nzb_name.as_deref(), Some("My Release"));
        assert_eq!(cfg.nzb_category.as_deref(), Some("TV > HD"));
        assert_eq!(cfg.nzb_dir.as_deref(), Some("/tmp/nzb"));
        assert!(!cfg.history);
        assert!(cfg.resume);
        assert_eq!(cfg.post_hook.as_deref(), Some("notify.sh"));
        assert!(cfg.nfo);
    }

    #[test]
    fn toml_compression_section_sets_format() {
        let file: FileConfig = toml::from_str(
            "[server]\nhost = \"h\"\n[posting]\ngroups = [\"a\"]\n[compression]\nformat = \"rar\"\n",
        )
        .unwrap();
        let cfg = Config::resolve(file, Overrides::default()).unwrap();
        assert_eq!(cfg.compress_format.as_deref(), Some("rar"));
    }

    #[test]
    fn toml_notify_section_sets_webhook_and_ntfy() {
        let file: FileConfig = toml::from_str(
            r#"
            [server]
            host = "h"
            [posting]
            groups = ["alt.test"]
            [notify]
            webhook_url = "https://discord.com/api/webhooks/x"
            ntfy_topic = "my-alerts"
            "#,
        )
        .unwrap();
        let cfg = Config::resolve(file, Overrides::default()).unwrap();
        assert_eq!(
            cfg.notify_webhook.as_deref(),
            Some("https://discord.com/api/webhooks/x")
        );
        assert_eq!(cfg.notify_ntfy.as_deref(), Some("my-alerts"));
    }

    #[test]
    fn toml_unknown_field_is_rejected() {
        let result: Result<FileConfig, _> =
            toml::from_str("[server]\nhost = \"h\"\nunknown_key = true\n");
        assert!(
            result.is_err(),
            "deny_unknown_fields should reject unknown keys"
        );
    }

    #[test]
    fn file_config_load_from_disk() {
        use std::io::Write as _;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            "[server]\nhost = \"disk-host\"\n[posting]\ngroups = [\"a\"]\n"
        )
        .unwrap();
        let loaded = FileConfig::load(f.path()).unwrap();
        assert_eq!(loaded.server.host.as_deref(), Some("disk-host"));
    }

    #[test]
    fn file_config_load_missing_file_errors() {
        let err = FileConfig::load(std::path::Path::new("/no/such/file.toml")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("reading config file"));
    }

    // ── CLI overrides ─────────────────────────────────────────────────────────

    #[test]
    fn cli_overrides_article_size_and_retries() {
        let mut file = FileConfig::default();
        file.server.host = Some("h".into());
        file.posting.groups = Some(vec!["a.b".into()]);
        file.posting.article_size = Some(500_000);
        file.posting.retries = Some(2);

        let cli = Overrides {
            article_size: Some(999_000),
            retries: Some(5),
            ..Default::default()
        };

        let cfg = Config::resolve(file, cli).unwrap();
        assert_eq!(cfg.article_size, 999_000);
        assert_eq!(cfg.retries, 5);
    }

    #[test]
    fn dry_run_does_not_require_host() {
        let mut file = FileConfig::default();
        file.posting.groups = Some(vec!["a.b".into()]);

        let cli = Overrides {
            dry_run: Some(true),
            ..Default::default()
        };

        let cfg = Config::resolve(file, cli).unwrap();
        assert!(cfg.dry_run);
        assert_eq!(cfg.host, "localhost");
    }

    #[test]
    fn par2_only_does_not_require_host_or_groups() {
        let file = FileConfig::default();

        let cli = Overrides {
            par2_only: Some(true),
            ..Default::default()
        };

        let cfg = Config::resolve(file, cli).unwrap();
        assert!(cfg.par2_only);
    }

    #[test]
    fn missing_groups_errors_for_normal_post() {
        let mut file = FileConfig::default();
        file.server.host = Some("h".into());
        // No groups set anywhere.

        assert!(Config::resolve(file, Overrides::default()).is_err());
    }

    #[test]
    fn cli_overrides_ssl_and_connections() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                ssl: Some(false),
                connections: Some(16),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!cfg.ssl);
        assert_eq!(cfg.connections, 16);
    }

    #[test]
    fn cli_overrides_username_and_password() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                username: Some("alice".into()),
                password: Some("hunter2".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.username.as_deref(), Some("alice"));
        assert_eq!(cfg.password.as_deref(), Some("hunter2"));
    }

    #[test]
    fn cli_overrides_line_length_and_retry_delay() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                line_length: Some(64),
                retry_delay: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.line_length, 64);
        assert_eq!(cfg.retry_delay, 10);
    }

    #[test]
    fn cli_overrides_obfuscate_and_par2() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                obfuscate: Some(ObfuscateMode::Full),
                par2: Some(25),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.obfuscate, ObfuscateMode::Full);
        assert_eq!(cfg.par2, 25);
    }

    #[test]
    fn cli_overrides_verify_resume_no_archive() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                verify: Some(true),
                resume: Some(true),
                no_archive: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(cfg.verify);
        assert!(cfg.resume);
        assert!(cfg.no_archive);
    }

    #[test]
    fn cli_overrides_date_and_message_id_domain() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                date: Some("random".into()),
                message_id_domain: Some("example.net".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.date.as_deref(), Some("random"));
        assert_eq!(cfg.message_id_domain.as_deref(), Some("example.net"));
    }

    #[test]
    fn cli_overrides_from_and_groups() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                from: Some("Bot <bot@x>".into()),
                groups: Some(vec!["alt.binaries.test".into(), "alt.test".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.from, "Bot <bot@x>");
        assert_eq!(cfg.groups, vec!["alt.binaries.test", "alt.test"]);
    }

    #[test]
    fn cli_overrides_upload_rate() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                upload_rate: Some(5 * 1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.upload_rate, 5 * 1024 * 1024);
    }

    #[test]
    fn cli_overrides_compress_format_and_password() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                compress_format: Some("zip".into()),
                compress_password: Some("pass123".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.compress_format.as_deref(), Some("zip"));
        assert_eq!(cfg.compress_password.as_deref(), Some("pass123"));
    }

    #[test]
    fn cli_overrides_nzb_metadata() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                nzb_name: Some("My Show S01".into()),
                nzb_password: Some("abc".into()),
                nzb_category: Some("TV".into()),
                nzb_dir: Some("/out".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.nzb_name.as_deref(), Some("My Show S01"));
        assert_eq!(cfg.nzb_password.as_deref(), Some("abc"));
        assert_eq!(cfg.nzb_category.as_deref(), Some("TV"));
        assert_eq!(cfg.nzb_dir.as_deref(), Some("/out"));
    }

    #[test]
    fn cli_overrides_history_and_nfo_and_post_hook() {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                history: Some(false),
                nfo: Some(true),
                post_hook: Some("notify.sh".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!cfg.history);
        assert!(cfg.nfo);
        assert_eq!(cfg.post_hook.as_deref(), Some("notify.sh"));
    }

    #[test]
    fn cli_upload_rate_wins_over_file_upload_rate() {
        let file: FileConfig = toml::from_str(
            "[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\nupload_rate=\"100 MiB/s\"\n",
        )
        .unwrap();
        let cfg = Config::resolve(
            file,
            Overrides {
                upload_rate: Some(1024),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.upload_rate, 1024);
    }

    #[test]
    fn file_upload_rate_used_when_cli_absent() {
        let file: FileConfig = toml::from_str(
            "[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\nupload_rate=\"1 KiB/s\"\n",
        )
        .unwrap();
        let cfg = Config::resolve(file, Overrides::default()).unwrap();
        assert_eq!(cfg.upload_rate, 1024);
    }
}
