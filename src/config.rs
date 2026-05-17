//! Configuration loading and resolution.
//!
//! Settings come from an optional TOML file; any field may be overridden by a
//! CLI flag. [`FileConfig`] mirrors the TOML layout (everything optional);
//! [`Config`] is the fully resolved, validated result used by the rest of the
//! program.

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Deserialize;
use std::path::Path;

/// Default NNTP-over-TLS port.
pub const DEFAULT_PORT: u16 = 563;
/// Default number of parallel connections.
pub const DEFAULT_CONNECTIONS: usize = 4;
/// Default target size of each article body, in bytes.
pub const DEFAULT_ARTICLE_SIZE: usize = 768_000;

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

/// Configuration as parsed from the TOML file. Every field is optional so the
/// file may be partial and the remainder supplied via CLI flags.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub auth: AuthSection,
    #[serde(default)]
    pub posting: PostingSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub ssl: Option<bool>,
    pub connections: Option<usize>,
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
    pub obfuscate: Option<ObfuscateMode>,
    pub par2: Option<u8>,
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
    pub obfuscate: Option<ObfuscateMode>,
    pub dry_run: Option<bool>,
    pub par2: Option<u8>,
    pub par2_only: Option<bool>,
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
    pub from: String,
    pub groups: Vec<String>,
    pub article_size: usize,
    /// How much of each post to obfuscate.
    pub obfuscate: ObfuscateMode,
    /// If true, skip the network and just simulate posting.
    pub dry_run: bool,
    /// Percentage of PAR2 recovery data to generate (0 to disable).
    pub par2: u8,
    /// Only generate PAR2 files without uploading them.
    pub par2_only: bool,
}

impl Config {
    /// Resolve a [`Config`] from an optional file config plus CLI overrides.
    ///
    /// Precedence, highest first: CLI override, file value, built-in default.
    /// Returns an error if a required field (host, from, groups) is missing.
    pub fn resolve(file: FileConfig, cli: Overrides) -> Result<Self> {
        let dry_run = cli.dry_run.unwrap_or(false);
        let par2_only = cli.par2_only.unwrap_or(false);

        let host = if dry_run || par2_only {
            cli.host
                .or(file.server.host)
                .unwrap_or_else(|| "localhost".into())
        } else {
            cli.host
                .or(file.server.host)
                .context("no `host` set: provide [server].host or --host")?
        };

        let from = if par2_only {
            cli.from.or(file.posting.from).unwrap_or_else(|| "none".into())
        } else {
            cli.from
                .or(file.posting.from)
                .context("no `from` set: provide [posting].from or --from")?
        };

        let groups = if par2_only {
            cli.groups.or(file.posting.groups).unwrap_or_else(|| vec!["none".into()])
        } else {
            cli.groups
                .or(file.posting.groups)
                .filter(|g| !g.is_empty())
                .context("no `groups` set: provide [posting].groups or --groups")?
        };

        Ok(Config {
            host,
            port: cli.port.or(file.server.port).unwrap_or(DEFAULT_PORT),
            ssl: cli.ssl.or(file.server.ssl).unwrap_or(true),
            connections: cli
                .connections
                .or(file.server.connections)
                .unwrap_or(DEFAULT_CONNECTIONS),
            username: cli.username.or(file.auth.username),
            password: cli.password.or(file.auth.password),
            from,
            groups,
            article_size: cli
                .article_size
                .or(file.posting.article_size)
                .unwrap_or(DEFAULT_ARTICLE_SIZE),
            obfuscate: cli.obfuscate.or(file.posting.obfuscate).unwrap_or_default(),
            dry_run,
            par2: cli.par2.or(file.posting.par2).unwrap_or(10),
            par2_only,
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
