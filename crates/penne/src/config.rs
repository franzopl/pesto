//! Download configuration: servers and local paths.
//!
//! Reuses [`pesto::config::ServerEntry`] for server credentials so `penne`
//! and `pesto` can share the same `[[servers]]` TOML block in a combined
//! config file, instead of redefining host/port/TLS/auth fields here.

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use pesto::config::{ServerEntry, DEFAULT_RETRIES, DEFAULT_RETRY_DELAY};
use serde::{Deserialize, Serialize};

/// Default number of parallel download connections.
pub const DEFAULT_CONNECTIONS: usize = 8;

/// Directory containing the default config file (its parent), when one can
/// be determined for this OS/environment.
pub fn config_dir() -> Option<PathBuf> {
    default_config_path().and_then(|p| p.parent().map(PathBuf::from))
}

/// Path of the config file `penne` loads when `--config` is given with no
/// value, or omitted entirely.
///
/// On Unix: follows the XDG Base Directory spec
/// (`$XDG_CONFIG_HOME/penne/config.toml`), falling back to
/// `$HOME/.config/penne/config.toml`. On Windows: `%APPDATA%\penne\config.toml`.
/// Mirrors `pesto::config::default_config_path`, one directory over.
pub fn default_config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("penne").join("config.toml"))
    }
    #[cfg(not(windows))]
    {
        config_path_from_env(
            std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()),
            std::env::var_os("HOME"),
        )
    }
}

/// Pure helper behind [`default_config_path`] on Unix, factored out so the
/// XDG-vs-`$HOME` fallback logic is testable without mutating process-global
/// environment variables (unsafe to do from parallel tests).
#[cfg(not(windows))]
fn config_path_from_env(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home {
        return Some(PathBuf::from(xdg).join("penne").join("config.toml"));
    }
    home.map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("penne")
            .join("config.toml")
    })
}

/// Fully resolved download configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Priority tiers to download from, in order (the first tier is
    /// primary, the rest are backfill for whatever segments it's missing).
    /// Each tier holds one or more servers sharing that priority — see
    /// [`ServerTier`].
    pub server_tiers: Vec<ServerTier>,
    /// Directory where completed files are written.
    pub download_dir: PathBuf,
    /// Default number of parallel NNTP connections for a server that
    /// doesn't set its own `connections`.
    pub connections: usize,
    /// Number of retry attempts per segment against a single server before
    /// moving on to the next configured server (see
    /// [`crate::download::download_queue`]). Each server's own
    /// `retry_delay` governs the pause between attempts.
    pub retries: u32,
}

/// One priority tier: one or more servers sharing the same priority,
/// drained together as a single combined worker pool (each member
/// contributing its own `connections`) rather than one strictly after
/// another.
///
/// `nzbget`'s `ServerPool` calls this a `level`+`group` pair (`ROADMAP.md`
/// Phase 15) — a `level` is the priority tier (what `penne` already
/// expressed purely through list position before this), and a `group`
/// pools servers *within* a level. `penne` doesn't need a separate numeric
/// `level`: [`RawConfig::resolve`] already derives tier order from
/// `[[servers]]`'s own order in the TOML file, so grouping only needs to
/// cluster *adjacent* entries that share a [`RawServer::group`] value —
/// see that field's doc comment for the exact rule.
#[derive(Debug, Clone)]
pub struct ServerTier {
    pub members: Vec<ServerEntry>,
}

impl ServerTier {
    /// A tier of exactly one server — today's behavior before grouping
    /// existed, and still what an ungrouped `[[servers]]` entry resolves
    /// to.
    pub fn solo(entry: ServerEntry) -> Self {
        ServerTier {
            members: vec![entry],
        }
    }
}

/// On-disk TOML representation of a `[[servers]]` entry, before defaults are
/// applied. Mirrors the subset of `pesto`'s server fields relevant to
/// downloading (no posting-only fields such as obfuscation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawServer {
    pub host: String,
    pub port: Option<u16>,
    #[serde(default)]
    pub ssl: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    pub connections: Option<usize>,
    /// Seconds to wait between retry attempts against this server.
    pub retry_delay: Option<u64>,
    /// Pool this server with the *adjacent* `[[servers]]` entries sharing
    /// the same `group` value: instead of one strictly finishing its pass
    /// before the next starts, every member's connections are drained
    /// together as one combined worker pool at that shared priority.
    /// Covers two equal-priority accounts (e.g. two blocks of connections
    /// on the same provider, or two mirror providers) that should share
    /// worker load rather than act as primary/backup. Omitted (the
    /// default) keeps this server its own solitary priority tier, exactly
    /// as before this field existed. Servers with the *same* `group` value
    /// that are **not** adjacent in the file each start their own tier
    /// instead of being pooled — list group members next to each other.
    #[serde(default)]
    pub group: Option<u32>,
}

/// On-disk TOML representation of the whole config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawConfig {
    #[serde(default)]
    pub servers: Vec<RawServer>,
    pub download_dir: Option<PathBuf>,
    /// Default `connections` for a `[[servers]]` entry that doesn't set its
    /// own.
    pub connections: Option<usize>,
    /// Retry attempts per segment per server before failing over.
    pub retries: Option<u32>,
}

impl RawConfig {
    /// Parse a TOML config file's contents.
    pub fn parse(contents: &str) -> Result<Self> {
        toml::from_str(contents).context("parsing penne config TOML")
    }

    /// Resolve into a fully-defaulted [`Config`].
    ///
    /// `download_dir` falls back to the current directory when neither the
    /// config file nor a CLI override provides one.
    pub fn resolve(self) -> Result<Config> {
        let default_connections = self.connections.unwrap_or(DEFAULT_CONNECTIONS);

        // Clusters *adjacent* `[[servers]]` entries sharing a `group` value
        // into one `ServerTier`; every other entry (an ungrouped one, or one
        // whose `group` differs from the tier being built) starts a new
        // solitary tier — see `RawServer::group`'s doc comment for why
        // adjacency is what's required, not just a matching value anywhere
        // in the file.
        let mut server_tiers: Vec<ServerTier> = Vec::new();
        let mut current_group: Option<u32> = None;
        for s in self.servers {
            let entry = ServerEntry {
                host: s.host,
                port: s.port.unwrap_or(if s.ssl { 563 } else { 119 }),
                ssl: s.ssl,
                connections: s.connections.unwrap_or(default_connections),
                username: s.username,
                password: s.password,
                retry_delay: s.retry_delay.unwrap_or(DEFAULT_RETRY_DELAY),
                timeout: 120,
            };
            let joins_current_tier =
                matches!((s.group, current_group), (Some(g), Some(cg)) if g == cg);
            if joins_current_tier {
                server_tiers
                    .last_mut()
                    .expect("current_group only set once a tier already exists")
                    .members
                    .push(entry);
            } else {
                server_tiers.push(ServerTier::solo(entry));
                current_group = s.group;
            }
        }

        Ok(Config {
            server_tiers,
            download_dir: self.download_dir.unwrap_or_else(|| PathBuf::from(".")),
            connections: default_connections,
            retries: self.retries.unwrap_or(DEFAULT_RETRIES),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_server_block() {
        let toml = r#"
            download_dir = "/downloads"

            [[servers]]
            host = "news.example.com"
            ssl = true
            username = "user"
            password = "pass"
        "#;
        let raw = RawConfig::parse(toml).unwrap();
        let config = raw.resolve().unwrap();
        assert_eq!(config.server_tiers.len(), 1);
        assert_eq!(config.server_tiers[0].members.len(), 1);
        assert_eq!(config.server_tiers[0].members[0].host, "news.example.com");
        assert_eq!(config.server_tiers[0].members[0].port, 563);
        assert_eq!(config.download_dir, PathBuf::from("/downloads"));
    }

    #[test]
    fn defaults_download_dir_to_current_directory() {
        let raw = RawConfig::parse("").unwrap();
        let config = raw.resolve().unwrap();
        assert_eq!(config.download_dir, PathBuf::from("."));
        assert!(config.server_tiers.is_empty());
    }

    #[test]
    fn top_level_connections_is_the_default_for_servers_without_their_own() {
        let toml = r#"
            connections = 20

            [[servers]]
            host = "primary.example.com"

            [[servers]]
            host = "backup.example.com"
            connections = 3
        "#;
        let config = RawConfig::parse(toml).unwrap().resolve().unwrap();
        // No `group` on either entry: each stays its own solitary tier,
        // exactly as before grouping existed.
        assert_eq!(config.server_tiers.len(), 2);
        assert_eq!(config.server_tiers[0].members[0].connections, 20);
        assert_eq!(config.server_tiers[1].members[0].connections, 3);
    }

    #[test]
    fn connections_defaults_to_the_built_in_default_when_unset_anywhere() {
        let toml = r#"
            [[servers]]
            host = "news.example.com"
        "#;
        let config = RawConfig::parse(toml).unwrap().resolve().unwrap();
        assert_eq!(
            config.server_tiers[0].members[0].connections,
            DEFAULT_CONNECTIONS
        );
    }

    #[test]
    fn adjacent_servers_sharing_a_group_are_pooled_into_one_tier() {
        let toml = r#"
            [[servers]]
            host = "account-a.example.com"
            group = 1
            connections = 5

            [[servers]]
            host = "account-b.example.com"
            group = 1
            connections = 3

            [[servers]]
            host = "backup.example.com"
        "#;
        let config = RawConfig::parse(toml).unwrap().resolve().unwrap();
        assert_eq!(config.server_tiers.len(), 2);
        assert_eq!(config.server_tiers[0].members.len(), 2);
        assert_eq!(
            config.server_tiers[0].members[0].host,
            "account-a.example.com"
        );
        assert_eq!(
            config.server_tiers[0].members[1].host,
            "account-b.example.com"
        );
        assert_eq!(config.server_tiers[1].members.len(), 1);
        assert_eq!(config.server_tiers[1].members[0].host, "backup.example.com");
    }

    #[test]
    fn non_adjacent_servers_sharing_a_group_are_not_pooled() {
        // account-a and account-c share group 1 but aren't next to each
        // other in the file — each starts its own tier instead, per
        // `RawServer::group`'s documented adjacency requirement.
        let toml = r#"
            [[servers]]
            host = "account-a.example.com"
            group = 1

            [[servers]]
            host = "account-b.example.com"
            group = 2

            [[servers]]
            host = "account-c.example.com"
            group = 1
        "#;
        let config = RawConfig::parse(toml).unwrap().resolve().unwrap();
        assert_eq!(config.server_tiers.len(), 3);
        assert!(config.server_tiers.iter().all(|t| t.members.len() == 1));
    }

    #[test]
    fn ungrouped_servers_each_get_their_own_solitary_tier() {
        let toml = r#"
            [[servers]]
            host = "a.example.com"

            [[servers]]
            host = "b.example.com"
        "#;
        let config = RawConfig::parse(toml).unwrap().resolve().unwrap();
        assert_eq!(config.server_tiers.len(), 2);
        assert!(config.server_tiers.iter().all(|t| t.members.len() == 1));
    }

    #[cfg(not(windows))]
    #[test]
    fn config_path_prefers_xdg_config_home_over_dollar_home() {
        let path = config_path_from_env(Some("/xdg".into()), Some("/home/user".into()));
        assert_eq!(path, Some(PathBuf::from("/xdg/penne/config.toml")));
    }

    #[cfg(not(windows))]
    #[test]
    fn config_path_falls_back_to_home_dot_config() {
        let path = config_path_from_env(None, Some("/home/user".into()));
        assert_eq!(
            path,
            Some(PathBuf::from("/home/user/.config/penne/config.toml"))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn config_path_none_when_neither_env_var_is_set() {
        assert_eq!(config_path_from_env(None, None), None);
    }
}
