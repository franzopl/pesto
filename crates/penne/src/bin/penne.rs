//! `penne` CLI: reads a `.nzb` and (eventually) downloads it.
//!
//! Currently only `info` is functional end-to-end; `download` parses the
//! `.nzb` and reports what it would do, since article retrieval is not
//! implemented yet (see `ROADMAP.md` Phase 2 onward).

use std::path::PathBuf;

use anyhow::Result;
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
    /// Download the contents of a `.nzb`.
    Download {
        /// Path to the `.nzb` file.
        nzb: PathBuf,
        /// Destination directory for completed files. Defaults to the
        /// config file's `download_dir`, or the current directory.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Path to a `penne` TOML config file.
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Command::Info { nzb } => {
            let parsed = penne::nzb::load(&nzb)?;
            let summary = penne::nzb::summarize(&parsed);
            println!("{}", nzb.display());
            println!("  poster:   {}", parsed.poster);
            println!("  groups:   {}", parsed.groups.join(", "));
            println!("  files:    {}", summary.files);
            println!("  segments: {}", summary.segments);
            println!("  size:     {} bytes", summary.total_bytes);
            Ok(())
        }
        Command::Download {
            nzb,
            out_dir,
            config,
        } => {
            let parsed = penne::nzb::load(&nzb)?;
            let summary = penne::nzb::summarize(&parsed);
            let queue = penne::queue::build(&parsed);

            let dest = out_dir
                .or_else(|| {
                    config
                        .as_ref()
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .and_then(|s| penne::config::RawConfig::parse(&s).ok())
                        .and_then(|c| c.download_dir)
                })
                .unwrap_or_else(|| PathBuf::from("."));

            println!(
                "would download {} file(s), {} segment(s), {} bytes to {}",
                queue.files.len(),
                summary.segments,
                summary.total_bytes,
                dest.display()
            );
            println!("download engine not implemented yet — see ROADMAP.md Phase 2 onward");
            Ok(())
        }
    }
}
