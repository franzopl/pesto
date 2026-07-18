//! `penne` CLI: reads a `.nzb`, downloads it, and assembles the result.
//!
//! `info` and `download` are both functional end-to-end as of Phase 4:
//! fetch (Phase 2), yEnc decode (Phase 3), and file assembly (Phase 4).
//! Concurrency is still one connection per server (`ROADMAP.md` Phase 2's
//! still-open item), and PAR2 repair / archive extraction are not wired up
//! yet (Phases 6/7).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "penne", version, about = "Fast NZB downloader")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a `.nzb` and print file/segment/size counts.
    Info {
        /// Path to the `.nzb` file.
        nzb: PathBuf,
    },
    /// Download and assemble the contents of a `.nzb`.
    Download {
        /// Path to the `.nzb` file.
        nzb: PathBuf,
        /// Destination directory for completed files. Defaults to the
        /// config file's `download_dir`, or the current directory.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Path to a `penne` TOML config file (server credentials).
        #[arg(long)]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Command::Info { nzb } => info(&nzb),
        Command::Download {
            nzb,
            out_dir,
            config,
        } => download(&nzb, out_dir, &config).await,
    }
}

fn info(nzb: &Path) -> Result<()> {
    let parsed = penne::nzb::load(nzb)?;
    let summary = penne::nzb::summarize(&parsed);
    println!("{}", nzb.display());
    println!("  poster:   {}", parsed.poster);
    println!("  groups:   {}", parsed.groups.join(", "));
    println!("  files:    {}", summary.files);
    println!("  segments: {}", summary.segments);
    println!("  size:     {} bytes", summary.total_bytes);
    Ok(())
}

async fn download(nzb: &Path, out_dir: Option<PathBuf>, config_path: &Path) -> Result<()> {
    let parsed = penne::nzb::load(nzb)?;
    let queue = penne::queue::build(&parsed);

    let config_toml = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = penne::config::RawConfig::parse(&config_toml)?.resolve()?;
    anyhow::ensure!(
        !config.servers.is_empty(),
        "no [[servers]] configured in {}",
        config_path.display()
    );
    let dest_dir = out_dir.unwrap_or(config.download_dir);

    let outcome = penne::download::download_queue(&queue, &config.servers, None).await?;
    println!(
        "fetched {} segment(s); {} missing; {} corrupt",
        outcome.segments.len(),
        outcome.missing.len(),
        outcome.corrupt.len()
    );
    for seg in &outcome.missing {
        println!("  missing: {} part {}", seg.file_name, seg.part);
    }
    for seg in &outcome.corrupt {
        println!(
            "  corrupt: {} part {} ({})",
            seg.file_name, seg.part, seg.error
        );
    }

    let assembled =
        penne::assemble::assemble_all(&queue, &outcome.segments, &dest_dir, None).await?;
    let mut incomplete = 0;
    for (name, result) in &assembled {
        match result {
            penne::assemble::AssembleOutcome::Complete => println!("  ok: {name}"),
            penne::assemble::AssembleOutcome::CompleteUnverified => {
                println!("  ok (unverified): {name}")
            }
            penne::assemble::AssembleOutcome::ChecksumMismatch { .. } => {
                incomplete += 1;
                println!("  DAMAGED: {name} ({result:?})");
            }
            penne::assemble::AssembleOutcome::Incomplete { .. } => {
                incomplete += 1;
                println!("  INCOMPLETE: {name} ({result:?})");
            }
        }
    }

    if incomplete > 0 {
        anyhow::bail!(
            "{incomplete} file(s) incomplete or damaged; PAR2 repair not wired up yet (ROADMAP.md Phase 6)"
        );
    }
    Ok(())
}
