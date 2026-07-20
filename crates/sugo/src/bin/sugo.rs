//! `sugo` binary: loads config, starts the background job worker, and
//! serves the SABnzbd-compatible API plus the htmx UI.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "sugo",
    version,
    about = "SABnzbd-API-compatible web UI for penne",
    long_about = "SABnzbd-API-compatible web UI for penne.\n\n\
Server credentials and the [web] section (bind address, API key, data \
directory) are read from a TOML config file. If --config is not given, \
sugo loads it from the OS-standard location: \
$XDG_CONFIG_HOME/sugo/config.toml (or ~/.config/sugo/config.toml) \
on Linux/macOS, or %APPDATA%\\sugo\\config.toml on Windows. It also \
accepts a plain `penne` config.toml unchanged — just add a [web] table."
)]
struct Cli {
    /// TOML config file. Defaults to the OS-standard sugo config path.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Override the [web].bind_addr from the config file.
    #[arg(long)]
    bind: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config_path = match cli.config {
        Some(path) => path,
        None => sugo::config::default_config_path()
            .context("cannot locate a config directory: set $HOME or $XDG_CONFIG_HOME")?,
    };

    let mut web_config = if config_path.exists() {
        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        sugo::config::WebConfig::parse(&contents)?
    } else {
        tracing::warn!(
            "no config found at {}; starting unconfigured (visit /settings once running, \
             or create the file with a [[servers]] entry and a [web].api_key)",
            config_path.display()
        );
        sugo::config::WebConfig::default()
    };

    if let Some(bind) = cli.bind {
        web_config.web.bind_addr = Some(bind);
    }

    let data_dir = web_config.data_dir();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;

    let bind_addr = web_config.bind_addr();
    let state = sugo::state::AppState::new(web_config, data_dir, Some(config_path));
    sugo::job::worker::spawn(state.clone());

    let router = sugo::build_router(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    tracing::info!("sugo listening on http://{bind_addr}");
    axum::serve(listener, router).await?;
    Ok(())
}
