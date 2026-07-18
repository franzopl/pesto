//! `penne` CLI: reads a `.nzb`, downloads it, assembles the result, and
//! verifies/repairs it with PAR2 if recovery data was part of the release.
//!
//! `info` and `download` are both functional end-to-end as of Phase 6:
//! fetch (Phase 2), yEnc decode (Phase 3), file assembly (Phase 4), and PAR2
//! verify/repair (Phase 6). Concurrency is still one connection per server
//! (`ROADMAP.md` Phase 2's still-open item), and archive extraction is not
//! wired up yet (Phase 7).

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

    Ok(())
}
