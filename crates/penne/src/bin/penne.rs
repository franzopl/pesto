//! `penne` CLI: reads a `.nzb`, downloads it, assembles the result,
//! verifies/repairs it with PAR2 if recovery data was part of the release,
//! and extracts any archives (`.rar`/`.7z`/`.zip`) it finds.
//!
//! `info` and `download` are both functional end-to-end: fetch (Phase 2,
//! with per-segment retry/backoff, resume via [`penne::cache`], and
//! N-parallel-connections-per-server concurrency — Phases 8/9), yEnc decode
//! (Phase 3), file assembly (Phase 4), PAR2 verify/repair (Phase 6), and
//! archive extraction (Phase 7).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "penne",
    version,
    about = "Fast NZB downloader",
    long_about = "Fast NZB downloader.\n\n\
Server credentials are read from a TOML config file. If --config is not \
given, penne loads it from the OS-standard location: $XDG_CONFIG_HOME/penne/config.toml \
(or, failing that, ~/.config/penne/config.toml) on Linux/macOS, or \
%APPDATA%\\penne\\config.toml on Windows. Create that file interactively \
with `penne --config`, or point at a specific file with `--config <FILE>`."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// TOML config file (server credentials, download directory). With no
    /// value (`penne --config`), launch the interactive setup wizard
    /// instead of running a command. When omitted entirely, the default
    /// config path is used.
    #[arg(long, global = true)]
    config: Option<Option<PathBuf>>,
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
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    // `penne --config` with no value: launch the interactive setup wizard,
    // regardless of whether a subcommand was also given.
    if matches!(cli.config, Some(None)) {
        return penne::wizard::run();
    }

    match cli.command {
        Some(Command::Info { nzb }) => info(&nzb),
        Some(Command::Download { nzb, out_dir }) => {
            download(&nzb, out_dir, cli.config.flatten()).await
        }
        None => {
            println!(
                "penne — fast NZB downloader.\n\n\
                 Run `penne --help` for usage, or `penne --config` to set up your servers."
            );
            Ok(())
        }
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

async fn download(
    nzb: &Path,
    out_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
) -> Result<()> {
    let parsed = penne::nzb::load(nzb)?;
    let queue = penne::queue::build(&parsed);

    let config_path = match config_path {
        Some(path) => path,
        None => {
            let default = penne::config::default_config_path()
                .context("cannot locate a config directory: set $HOME or $XDG_CONFIG_HOME")?;
            anyhow::ensure!(
                default.exists(),
                "no config found at {}; run `penne --config` to create one, or pass --config <FILE>",
                default.display()
            );
            eprintln!("using config: {}", default.display());
            default
        }
    };
    let config_toml = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = penne::config::RawConfig::parse(&config_toml)?.resolve()?;
    anyhow::ensure!(
        !config.servers.is_empty(),
        "no [[servers]] configured in {}",
        config_path.display()
    );
    let dest_dir = out_dir.unwrap_or(config.download_dir);

    let outcome =
        penne::download::download_queue(&queue, &config.servers, &dest_dir, config.retries, None)
            .await?;
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
    let mut needs_repair = 0u32;
    for (name, result) in &assembled {
        match result {
            penne::assemble::AssembleOutcome::Complete => println!("  ok: {name}"),
            penne::assemble::AssembleOutcome::CompleteUnverified => {
                println!("  ok (unverified): {name}")
            }
            penne::assemble::AssembleOutcome::ChecksumMismatch { .. } => {
                needs_repair += 1;
                println!("  damaged (will attempt PAR2 repair): {name} ({result:?})");
            }
            penne::assemble::AssembleOutcome::Incomplete { .. } => {
                needs_repair += 1;
                println!("  incomplete (will attempt PAR2 repair): {name} ({result:?})");
            }
        }
    }

    match penne::repair::verify_and_repair(&dest_dir).await? {
        penne::repair::RepairOutcome::Ok => println!("PAR2: all files verified intact"),
        penne::repair::RepairOutcome::Repaired(plan) => {
            for f in &plan.repaired_files {
                println!(
                    "  PAR2 repaired: {} ({} slice(s))",
                    f.name, f.slices_repaired
                );
            }
        }
        penne::repair::RepairOutcome::NotRepairable(report) => {
            anyhow::bail!(
                "{} damaged slice(s) exceed available PAR2 recovery data ({} block(s)); download is incomplete",
                report.total_bad_slices(),
                report.available_recovery_blocks
            );
        }
        penne::repair::RepairOutcome::NoRecoveryData => {
            anyhow::ensure!(
                needs_repair == 0,
                "{needs_repair} file(s) incomplete or damaged, and no PAR2 recovery data was found to repair them"
            );
        }
    }

    let extracted = penne::extract::extract_all(&dest_dir, parsed.meta.password.as_deref()).await?;
    for archive in &extracted {
        println!("  extracted: {} ({:?})", archive.base_name, archive.kind);
    }

    // Everything that needed fixing got fixed (we'd have bailed above
    // otherwise), so the cached article bodies kept for resume are no
    // longer needed.
    penne::cache::clear(&dest_dir)?;

    Ok(())
}
