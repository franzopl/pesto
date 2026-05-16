//! `pesto` — fast, lean Usenet poster.
//!
//! Parses the CLI, resolves the configuration, posts the given files to Usenet
//! and writes an `.nzb` file describing the result.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use pesto::config::{Config, FileConfig, Overrides};

/// Fast, lean Usenet poster: yEnc-encode files, post over NNTP, emit an .nzb.
#[derive(Parser, Debug)]
#[command(name = "pesto", version, about)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// NNTP server hostname.
    #[arg(long)]
    host: Option<String>,

    /// NNTP server port.
    #[arg(long)]
    port: Option<u16>,

    /// Disable TLS (overrides the config file).
    #[arg(long)]
    no_ssl: bool,

    /// Number of parallel connections.
    #[arg(long)]
    connections: Option<usize>,

    /// Authentication username.
    #[arg(long)]
    username: Option<String>,

    /// Authentication password.
    #[arg(long)]
    password: Option<String>,

    /// `From` header used on posted articles.
    #[arg(long)]
    from: Option<String>,

    /// Newsgroups to post to (repeat or comma-separate).
    #[arg(long, value_delimiter = ',')]
    groups: Vec<String>,

    /// Path of the `.nzb` file to write.
    #[arg(short, long, value_name = "PATH")]
    out: Option<PathBuf>,

    /// Post under random subjects and yEnc file names.
    #[arg(long)]
    obfuscate: bool,

    /// Files to post.
    #[arg(required = true, value_name = "FILE")]
    files: Vec<PathBuf>,
}

impl Cli {
    /// Build config [`Overrides`] from the parsed flags.
    fn overrides(&self) -> Overrides {
        Overrides {
            host: self.host.clone(),
            port: self.port,
            // `--no-ssl` is the only TLS flag; absent means "defer to config".
            ssl: if self.no_ssl { Some(false) } else { None },
            connections: self.connections,
            username: self.username.clone(),
            password: self.password.clone(),
            from: self.from.clone(),
            groups: if self.groups.is_empty() {
                None
            } else {
                Some(self.groups.clone())
            },
            article_size: None,
            obfuscate: if self.obfuscate { Some(true) } else { None },
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let file_config = match &cli.config {
        Some(path) => FileConfig::load(path)?,
        None => FileConfig::default(),
    };

    let config = Config::resolve(file_config, cli.overrides())?;

    let outcome = pesto::poster::post_files(&config, &cli.files).await?;

    println!("posted {} segment(s)", outcome.segments.len());
    if outcome.cancelled {
        eprintln!("interrupted — stopped before posting every requested segment");
    }
    if !outcome.failures.is_empty() {
        eprintln!("{} segment(s) failed:", outcome.failures.len());
        for failure in &outcome.failures {
            eprintln!("  - {failure}");
        }
    }

    if let Some(out) = &cli.out {
        if outcome.segments.is_empty() {
            eprintln!("no segments posted — skipping nzb output");
        } else {
            let xml = pesto::nzb::generate(&config.from, &config.groups, &outcome.segments);
            tokio::fs::write(out, xml)
                .await
                .with_context(|| format!("writing nzb file `{}`", out.display()))?;
            println!("wrote nzb: {}", out.display());
        }
    }

    // Exit codes: 130 for an interrupt, 1 for any failed segment, 0 otherwise.
    if outcome.cancelled {
        std::process::exit(130);
    }
    if !outcome.failures.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}
