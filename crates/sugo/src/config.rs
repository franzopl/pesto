//! Configuration for `sugo`: reuses [`penne::config::RawConfig`] for
//! server credentials (so an existing `penne` `config.toml` works unchanged
//! as `sugo`'s config too — just add a `[web]` table to it) plus a
//! `[web]` section for the HTTP server's own settings.
//!
//! Parsed as two independent passes over the same TOML text rather than one
//! `#[serde(flatten)]`ed struct: `toml`'s flatten support has known gaps for
//! structs containing `Vec<T>`/nested-table fields (`RawConfig::servers`),
//! and a struct without `deny_unknown_fields` already ignores whatever
//! top-level keys it doesn't recognize — so parsing into [`RawConfig`] and a
//! tiny `[web]`-only struct separately is both simpler and more robust than
//! fighting that limitation.

use std::path::PathBuf;

use anyhow::{Context, Result};
use penne::config::RawConfig;
use serde::{Deserialize, Serialize};

/// Default bind address when `[web].bind_addr` isn't set.
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8085";

/// The `[web]` TOML table.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebSection {
    pub bind_addr: Option<String>,
    /// Required to call the `/api` endpoint or view the UI. `None` means no
    /// key has been configured yet (the setup wizard/settings page should
    /// prompt for one) — treated as "reject every request" by
    /// [`crate::api::auth`], never as "open access".
    pub api_key: Option<String>,
    /// Where job state (`state.json`), uploaded `.nzb` staging files, and
    /// (unless `[core].download_dir` overrides it) completed downloads live.
    pub data_dir: Option<PathBuf>,
}

/// Resolved configuration: server credentials/download settings ([`penne`]'s
/// own [`RawConfig`], unmodified) plus this crate's own `[web]` table.
#[derive(Debug, Clone, Default)]
pub struct WebConfig {
    pub core: RawConfig,
    pub web: WebSection,
}

/// Only used to pull the `[web]` table out of the same TOML text handed to
/// [`RawConfig::parse`] — see the module doc comment for why this is a
/// second, separate parse rather than one flattened struct.
#[derive(Debug, Deserialize, Default)]
struct WebOnly {
    #[serde(default)]
    web: WebSection,
}

impl WebConfig {
    pub fn parse(contents: &str) -> Result<Self> {
        let core = RawConfig::parse(contents)?;
        let web = toml::from_str::<WebOnly>(contents)
            .context("parsing sugo [web] config table")?
            .web;
        Ok(WebConfig { core, web })
    }

    pub fn bind_addr(&self) -> String {
        self.web
            .bind_addr
            .clone()
            .unwrap_or_else(|| DEFAULT_BIND_ADDR.to_string())
    }

    pub fn api_key(&self) -> Option<&str> {
        self.web.api_key.as_deref()
    }

    pub fn data_dir(&self) -> PathBuf {
        self.web
            .data_dir
            .clone()
            .unwrap_or_else(|| default_data_dir().unwrap_or_else(|| PathBuf::from(".")))
    }

    /// Serialize back to TOML text — [`crate::web::settings`]'s "add server"
    /// form writes the result back to `config_path`. `[[servers]]` (and
    /// every other `core` field) round-trips through `RawConfig`'s own
    /// `Serialize` impl; the `[web]` table is appended by hand since `web`
    /// intentionally isn't part of that flattened struct (see the module
    /// doc comment).
    pub fn to_toml(&self) -> Result<String> {
        let mut out =
            toml::to_string_pretty(&self.core).context("serializing [[servers]]/core config")?;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\n[web]\n");
        if let Some(v) = &self.web.bind_addr {
            out.push_str(&format!("bind_addr = {v:?}\n"));
        }
        if let Some(v) = &self.web.api_key {
            out.push_str(&format!("api_key = {v:?}\n"));
        }
        if let Some(v) = &self.web.data_dir {
            out.push_str(&format!("data_dir = {:?}\n", v.to_string_lossy()));
        }
        Ok(out)
    }
}

/// Directory containing the default config file (its parent).
pub fn config_dir() -> Option<PathBuf> {
    default_config_path().and_then(|p| p.parent().map(PathBuf::from))
}

/// Default config path, mirroring [`penne::config::default_config_path`] one
/// directory over: `$XDG_CONFIG_HOME/sugo/config.toml` (falling back to
/// `$HOME/.config/sugo/config.toml`) on Unix, or
/// `%APPDATA%\sugo\config.toml` on Windows.
pub fn default_config_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(|appdata| PathBuf::from(appdata).join("sugo").join("config.toml"))
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(xdg).join("sugo").join("config.toml"));
        }
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join(".config")
                .join("sugo")
                .join("config.toml")
        })
    }
}

/// Default data directory (job state, uploaded `.nzb` staging, and —
/// unless overridden — completed downloads), following the XDG Base
/// Directory spec on Unix (`$XDG_DATA_HOME/sugo`, falling back to
/// `$HOME/.local/share/sugo`) or `%APPDATA%\sugo\data` on Windows.
pub fn default_data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("sugo").join("data"))
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(xdg).join("sugo"));
        }
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/sugo"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_servers_and_web_table_from_one_file() {
        let toml = r#"
            download_dir = "/downloads"

            [[servers]]
            host = "news.example.com"
            username = "user"
            password = "pass"

            [web]
            bind_addr = "0.0.0.0:9000"
            api_key = "secret"
        "#;
        let config = WebConfig::parse(toml).unwrap();
        assert_eq!(config.core.servers.len(), 1);
        assert_eq!(config.core.servers[0].host, "news.example.com");
        assert_eq!(config.bind_addr(), "0.0.0.0:9000");
        assert_eq!(config.api_key(), Some("secret"));
    }

    #[test]
    fn missing_web_table_falls_back_to_defaults() {
        let config = WebConfig::parse("").unwrap();
        assert_eq!(config.bind_addr(), DEFAULT_BIND_ADDR);
        assert_eq!(config.api_key(), None);
    }
}
