//! Download configuration: servers and local paths.
//!
//! Reuses [`pesto::config::ServerEntry`] for server credentials so `penne`
//! and `pesto` can share the same `[[servers]]` TOML block in a combined
//! config file, instead of redefining host/port/TLS/auth fields here.

use std::path::PathBuf;

use anyhow::{Context, Result};
use pesto::config::{ServerEntry, DEFAULT_RETRIES, DEFAULT_RETRY_DELAY};
use serde::{Deserialize, Serialize};

/// Default number of parallel download connections.
pub const DEFAULT_CONNECTIONS: usize = 8;

/// Fully resolved download configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Servers to download from, in priority order (first is primary, the
    /// rest are used as backfill for missing articles).
    pub servers: Vec<ServerEntry>,
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
        let servers = self
            .servers
            .into_iter()
            .map(|s| ServerEntry {
                host: s.host,
                port: s.port.unwrap_or(if s.ssl { 563 } else { 119 }),
                ssl: s.ssl,
                connections: s.connections.unwrap_or(default_connections),
                username: s.username,
                password: s.password,
                retry_delay: s.retry_delay.unwrap_or(DEFAULT_RETRY_DELAY),
                timeout: 120,
            })
            .collect();

        Ok(Config {
            servers,
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
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host, "news.example.com");
        assert_eq!(config.servers[0].port, 563);
        assert_eq!(config.download_dir, PathBuf::from("/downloads"));
    }

    #[test]
    fn defaults_download_dir_to_current_directory() {
        let raw = RawConfig::parse("").unwrap();
        let config = raw.resolve().unwrap();
        assert_eq!(config.download_dir, PathBuf::from("."));
        assert!(config.servers.is_empty());
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
        assert_eq!(config.servers[0].connections, 20);
        assert_eq!(config.servers[1].connections, 3);
    }

    #[test]
    fn connections_defaults_to_the_built_in_default_when_unset_anywhere() {
        let toml = r#"
            [[servers]]
            host = "news.example.com"
        "#;
        let config = RawConfig::parse(toml).unwrap().resolve().unwrap();
        assert_eq!(config.servers[0].connections, DEFAULT_CONNECTIONS);
    }
}
