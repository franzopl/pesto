use crate::article::random_from;
use crate::config::types::*;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

impl FileConfig {
    /// Load and parse a TOML config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file `{}`", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config file `{}`", path.display()))
    }
}

/// Path of the config file `pesto` loads when `--config` is not given.
pub fn config_dir() -> Option<PathBuf> {
    default_config_path().and_then(|p| p.parent().map(PathBuf::from))
}

/// On Unix: follows the XDG Base Directory spec (`$XDG_CONFIG_HOME/pesto/config.toml`),
/// falling back to `$HOME/.config/pesto/config.toml`.
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

impl Config {
    /// Resolve a [`Config`] from an optional file config plus CLI overrides.
    pub fn resolve(file: FileConfig, cli: Overrides) -> Result<Self> {
        let dry_run = cli.dry_run.unwrap_or(false);
        let par2_only = cli.par2_only.unwrap_or(false);

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
            par2_memory_limit: if let Some(s) = file.posting.par2_memory_limit {
                Some(parse_upload_rate(&s).with_context(|| "parsing par2_memory_limit")? as usize)
            } else {
                None
            },
            par2_only,
            verify: cli.verify.or(file.posting.verify).unwrap_or(false),
            resume: cli
                .resume
                .unwrap_or_else(|| file.output.resume.unwrap_or(false)),
            upload_rate: {
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
            history_dir: file.output.history_dir.map(|s| {
                if s.starts_with("~/") {
                    std::env::var_os("HOME")
                        .map(|h| PathBuf::from(h).join(&s[2..]))
                        .unwrap_or_else(|| PathBuf::from(&s))
                } else {
                    PathBuf::from(&s)
                }
            }),
            notify_webhook: file.notify.webhook_url,
            notify_ntfy: file.notify.ntfy_topic,
            notify: cli.notify,
            date: cli.date.or(file.posting.date),
            no_archive: cli.no_archive.or(file.posting.no_archive).unwrap_or(false),
            message_id_domain: cli.message_id_domain.or(file.posting.message_id_domain),
            post_hook: cli.post_hook.or(file.output.post_hook),
            no_hooks: cli.no_hooks,
            nfo: cli.nfo.unwrap_or_else(|| file.output.nfo.unwrap_or(false)),
            quiet: file.output.quiet.unwrap_or(false),
            bell: file.output.bell.unwrap_or(false),
            check: cli
                .check
                .unwrap_or_else(|| file.posting.check.unwrap_or(false)),
            check_delay_secs: cli
                .check_delay_secs
                .unwrap_or_else(|| file.posting.check_delay.unwrap_or(30)),
            check_retries: cli
                .check_retries
                .unwrap_or_else(|| file.posting.check_retries.unwrap_or(2)),
        })
    }
}
