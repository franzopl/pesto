use crate::article::random_from;
use crate::config::types::*;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Maps every known config field to the section it actually lives in, so an
/// `unknown field` error (raised by `#[serde(deny_unknown_fields)]`) can
/// point the user at the right place instead of just naming what's wrong —
/// e.g. `temp_dir` and `par2_temp_dir` are easy to swap between
/// `[compression]` and `[posting]`, or to drop under `[output]` by analogy
/// with `history_dir`.
const FIELD_SECTIONS: &[(&str, &str)] = &[
    // [server]
    ("host", "[server]"),
    ("port", "[server]"),
    ("ssl", "[server]"),
    ("connections", "[server]"),
    ("retry_delay", "[server]"),
    ("timeout", "[server]"),
    ("keepalive", "[server]"),
    // [auth]
    ("username", "[auth]"),
    ("password", "[auth]"),
    // [posting]
    ("from", "[posting]"),
    ("groups", "[posting]"),
    ("article_size", "[posting]"),
    ("line_length", "[posting]"),
    ("retries", "[posting]"),
    ("obfuscate", "[posting]"),
    ("par2", "[posting]"),
    ("upload_rate", "[posting]"),
    ("date", "[posting]"),
    ("no_archive", "[posting]"),
    ("file_counter", "[posting]"),
    ("message_id_domain", "[posting]"),
    ("check", "[posting]"),
    ("check_delay", "[posting]"),
    ("check_retries", "[posting]"),
    ("check_connections", "[posting]"),
    ("check_post_retries", "[posting]"),
    ("allow_incomplete_nzb", "[posting]"),
    ("check_recover_percent", "[posting]"),
    ("check_recover_max", "[posting]"),
    ("pipeline_depth", "[posting]"),
    ("par2_memory_limit", "[posting]"),
    ("memory_limit", "[posting]"),
    ("par2_temp_dir", "[posting]"),
    ("par2_before_upload", "[posting]"),
    // [output]
    ("history", "[output]"),
    ("history_dir", "[output]"),
    ("session_log", "[output]"),
    ("nzb", "[output]"),
    ("nzb_dir", "[output]"),
    ("nzb_title", "[output]"),
    ("nzb_name", "[output]"), // deprecated alias of nzb_title
    ("nzb_password", "[output]"),
    ("nzb_category", "[output]"),
    ("nzb_tags", "[output]"),
    ("pre_hook", "[output]"),
    ("pre_hooks", "[output]"),
    ("post_hook", "[output]"),
    ("post_hooks", "[output]"),
    ("no_hooks", "[output]"),
    ("nfo", "[output]"),
    ("nzb_conflict", "[output]"),
    ("resume", "[output]"),
    ("quiet", "[output]"),
    ("bell", "[output]"),
    // [output.indexer]
    ("url", "[output.indexer]"),
    ("api_key", "[output.indexer]"),
    // [compression]
    ("format", "[compression]"),
    ("temp_dir", "[compression]"),
    ("volume_size", "[compression]"),
    // [notify]
    ("webhook_url", "[notify]"),
    ("ntfy_topic", "[notify]"),
];

/// Given a TOML deserialize error, if it's an `unknown field` rejection for a
/// field name we recognise under a different section, return a hint
/// pointing at that section.
fn section_hint(err: &toml::de::Error) -> Option<String> {
    let field = err.message().strip_prefix("unknown field `")?;
    let field = &field[..field.find('`')?];
    let (_, section) = FIELD_SECTIONS.iter().find(|(name, _)| *name == field)?;
    Some(format!("`{field}` belongs under {section}, not here"))
}

impl FileConfig {
    /// Load and parse a TOML config file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file `{}`", path.display()))?;
        toml::from_str(&text)
            .map_err(|e| match section_hint(&e) {
                Some(hint) => anyhow::Error::new(e).context(hint),
                None => anyhow::Error::new(e),
            })
            .with_context(|| format!("parsing config file `{}`", path.display()))
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
        let proxy = cli
            .proxy
            .or(file.proxy)
            .or(file.server.proxy.clone())
            .as_deref()
            .map(Socks5Proxy::parse)
            .transpose()
            .context("parsing SOCKS5 proxy")?;
        let proxy_check_ip = cli.proxy_check_ip.unwrap_or(false);

        let tmdb = cli
            .tmdb
            .as_deref()
            .map(crate::nzb::parse_tmdb_ref)
            .transpose()
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| "parsing --tmdb")?;
        let imdb_id = cli
            .imdb_id
            .as_deref()
            .map(crate::nzb::parse_imdb_ref)
            .transpose()
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| "parsing --imdb-id")?;
        let tvdb = cli
            .tvdb_id
            .as_deref()
            .map(crate::nzb::parse_tvdb_ref)
            .transpose()
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| "parsing --tvdb-id")?;
        let mal_id = cli
            .mal_id
            .as_deref()
            .map(crate::nzb::parse_mal_ref)
            .transpose()
            .map_err(|e| anyhow::anyhow!(e))
            .with_context(|| "parsing --mal-id")?;

        let (host, port, ssl, connections, username, password, retry_delay, timeout, extra_servers) =
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
                let timeout = primary.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
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
                            // Per-entry timeout, falling back to the primary's.
                            proxy: proxy.clone(),
                            timeout: e.timeout.unwrap_or(timeout),
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
                    timeout,
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
                    file.server.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS),
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
        super::validation::validate_groups(&groups)?;

        let obfuscate = cli.obfuscate.or(file.posting.obfuscate).unwrap_or_default();
        let file_counter = cli
            .file_counter
            .or(file.posting.file_counter)
            .unwrap_or_else(|| obfuscate.policy().allow_file_counter);
        if file_counter && !obfuscate.policy().allow_file_counter {
            anyhow::bail!(
                "file_counter=true contradicts --obfuscate={}; use full-shared or light when release-wide grouping is required",
                match obfuscate {
                    ObfuscateMode::Full => "full",
                    ObfuscateMode::HeaderFragmented => "header-fragmented",
                    ObfuscateMode::Article => "article",
                    _ => unreachable!("only private modes reject file counters"),
                }
            );
        }
        let date = cli.date.or(file.posting.date);
        if date.as_deref() == Some("random") {
            eprintln!(
                "warning: posting.date=random is deprecated; omit Date or use `now`/a fixed RFC 2822 value"
            );
        }
        let message_id_domain = cli.message_id_domain.or(file.posting.message_id_domain);
        if let Some(domain) = &message_id_domain {
            if !crate::article::valid_message_id_domain(domain) {
                anyhow::bail!("invalid message_id_domain `{domain}`");
            }
        }

        Ok(Config {
            host,
            port,
            ssl,
            connections,
            username,
            password,
            proxy,
            proxy_check_ip,
            retry_delay,
            timeout,
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
            obfuscate,
            dry_run,
            par2: cli.par2.or(file.posting.par2).unwrap_or(DEFAULT_PAR2),
            par2_memory_limit: {
                if let Some(limit) = cli.par2_memory_limit {
                    Some(limit as usize)
                } else if let Some(s) = file.posting.par2_memory_limit {
                    Some(
                        parse_upload_rate(&s).with_context(|| "parsing par2_memory_limit")?
                            as usize,
                    )
                } else {
                    None
                }
            },
            memory_limit: {
                if let Some(limit) = cli.memory_limit {
                    Some(limit)
                } else if let Some(s) = file.posting.memory_limit {
                    parse_memory_limit_spec(&s).with_context(|| "parsing posting.memory_limit")?
                } else {
                    None
                }
            },
            par2_temp_dir: cli.par2_temp_dir.or(file.posting.par2_temp_dir).map(|s| {
                if let Some(rest) = s.strip_prefix("~/") {
                    std::env::var_os("HOME")
                        .map(|h| PathBuf::from(h).join(rest))
                        .unwrap_or_else(|| PathBuf::from(&s))
                } else {
                    PathBuf::from(&s)
                }
            }),
            par2_slice_size: cli.par2_slice_size.map(|s| s as usize),
            par2_slice_count: cli.par2_slice_count,
            par2_recovery_count: cli.par2_recovery_count,
            par2_only,
            par2_before_upload: cli
                .par2_before_upload
                .or(file.posting.par2_before_upload)
                .unwrap_or(false),
            threads: cli.threads.unwrap_or(0), // 0 means auto
            simd: cli.simd.unwrap_or_default(),
            resume: cli.resume.or(file.output.resume).unwrap_or(false),
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
            compress_temp_dir: cli
                .compress_temp_dir
                .or(file.compression.temp_dir)
                .map(|s| {
                    if let Some(rest) = s.strip_prefix("~/") {
                        std::env::var_os("HOME")
                            .map(|h| PathBuf::from(h).join(rest))
                            .unwrap_or_else(|| PathBuf::from(&s))
                    } else {
                        PathBuf::from(&s)
                    }
                }),
            compress_password: cli.compress_password,
            compress_volume_size: cli.compress_volume_size.or(file.compression.volume_size),
            nzb_title: cli.nzb_title.or_else(|| {
                file.output.nzb_title.or_else(|| {
                    file.output.nzb_name.inspect(|_| {
                        eprintln!(
                            "warning: config.toml's [output] nzb_name is deprecated, use \
                             nzb_title instead; nzb_name will stop being accepted in a \
                             future release"
                        );
                    })
                })
            }),
            nzb_password: cli.nzb_password.or(file.output.nzb_password),
            nzb_category: {
                let explicit = cli.nzb_category.or(file.output.nzb_category);
                explicit
                    .or_else(|| {
                        tmdb.as_ref()
                            .map(|(kind, _)| kind.default_category().to_string())
                    })
                    .or_else(|| {
                        tvdb.as_ref()
                            .map(|(kind, _)| kind.default_category().to_string())
                    })
            },
            nzb_tags: if cli.nzb_tags.is_empty() {
                file.output.nzb_tags
            } else {
                cli.nzb_tags
            },
            tmdb_id: tmdb
                .as_ref()
                .map(|(kind, id)| crate::nzb::format_tmdb_ref(*kind, id)),
            tmdb_kind: tmdb.as_ref().map(|(kind, _)| *kind),
            imdb_id,
            tvdb_id: tvdb.as_ref().map(|(_, id)| id.clone()),
            tvdb_kind: tvdb.as_ref().map(|(kind, _)| *kind),
            mal_id,
            nzb_dir: cli.nzb_dir.or(file.output.nzb_dir),
            indexer_url: file.output.indexer.url,
            indexer_api_key: file.output.indexer.api_key,
            history: cli.history.or(file.output.history).unwrap_or(true),
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
            date,
            no_archive: cli.no_archive.or(file.posting.no_archive).unwrap_or(false),
            file_counter,
            message_id_domain,
            pre_hooks: {
                // CLI flags take precedence over config file; single `pre_hook`
                // and array `pre_hooks` are merged so old configs still work.
                if !cli.pre_hooks.is_empty() {
                    cli.pre_hooks
                } else {
                    file.output
                        .pre_hook
                        .into_iter()
                        .chain(file.output.pre_hooks)
                        .collect()
                }
            },
            post_hooks: {
                if !cli.post_hooks.is_empty() {
                    cli.post_hooks
                } else {
                    file.output
                        .post_hook
                        .into_iter()
                        .chain(file.output.post_hooks)
                        .collect()
                }
            },
            no_hooks: cli.no_hooks.or(file.output.no_hooks).unwrap_or(false),
            nfo: cli.nfo.or(file.output.nfo).unwrap_or(false),
            nzb_conflict: cli
                .nzb_conflict
                .or(file.output.nzb_conflict)
                .unwrap_or_default(),
            quiet: file.output.quiet.unwrap_or(false),
            bell: file.output.bell.unwrap_or(false),
            check_delay_secs: cli
                .check_delay_secs
                .or(file.posting.check_delay)
                .unwrap_or(5),
            check: cli.check.or(file.posting.check).unwrap_or(true),
            check_retries: cli
                .check_retries
                .or(file.posting.check_retries)
                .unwrap_or(3),
            check_connections: cli
                .check_connections
                .or(file.posting.check_connections)
                .unwrap_or(0),
            check_post_retries: cli
                .check_post_retries
                .or(file.posting.check_post_retries)
                .unwrap_or(1),
            allow_incomplete_nzb: cli
                .allow_incomplete_nzb
                .or(file.posting.allow_incomplete_nzb)
                .unwrap_or(false),
            check_recover_percent: cli
                .check_recover_percent
                .or(file.posting.check_recover_percent)
                .unwrap_or(15),
            check_recover_max: cli
                .check_recover_max
                .or(file.posting.check_recover_max)
                .unwrap_or(50),
            // 0 = adaptive; any positive value is the explicit fixed depth.
            pipeline_depth: cli
                .pipeline_depth
                .or(file.posting.pipeline_depth)
                .unwrap_or(DEFAULT_PIPELINE_DEPTH),
            keepalive_interval: file.server.keepalive.unwrap_or(DEFAULT_KEEPALIVE_SECS),
        })
    }
}
