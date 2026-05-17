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
        std::env::var_os("APPDATA").map(|appdata| {
            PathBuf::from(appdata).join("pesto").join("config.toml")
        })
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
                    host, port, ssl, connections, username, password, retry_delay, extras,
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
            verify: cli
                .verify
                .or(file.posting.verify)
                .unwrap_or(false),
            resume: cli.resume.unwrap_or_else(|| file.output.resume.unwrap_or(false)),
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
            history: cli.history.unwrap_or_else(|| file.output.history.unwrap_or(true)),
            notify_webhook: file.notify.webhook_url,
            notify_ntfy: file.notify.ntfy_topic,
            notify: cli.notify,
            date: cli.date.or(file.posting.date),
            no_archive: cli.no_archive.or(file.posting.no_archive).unwrap_or(false),
            message_id_domain: cli.message_id_domain.or(file.posting.message_id_domain),
            post_hook: cli.post_hook.or(file.output.post_hook),
            nfo: cli.nfo.unwrap_or_else(|| file.output.nfo.unwrap_or(false)),
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
}
