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
        /// Archive extraction password. Overrides the `.nzb`'s own
        /// `<meta type="password">`, if any — useful for obfuscated
        /// releases that don't carry the password in the `.nzb` itself.
        #[arg(long)]
        password: Option<String>,
        /// Only check that every segment is still present on the
        /// configured server(s), via `STAT` (RFC 3977 §6.2.4) — no
        /// download, decode, PAR2, or extraction. Much cheaper over the
        /// wire than a real download, since STAT never transfers the
        /// article body.
        #[arg(long)]
        stat: bool,
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
        Some(Command::Download {
            nzb,
            out_dir,
            password,
            stat,
        }) => download(&nzb, out_dir, cli.config.flatten(), password, stat).await,
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
    password: Option<String>,
    stat: bool,
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
        !config.server_tiers.is_empty(),
        "no [[servers]] configured in {}",
        config_path.display()
    );

    if stat {
        return check_availability(&queue, &config.server_tiers, config.retries).await;
    }

    let dest_dir = out_dir.unwrap_or(config.download_dir);

    let required = penne::diskspace::required_bytes(&queue);
    let space = penne::diskspace::check(&dest_dir, required)?;
    anyhow::ensure!(
        space.is_enough(),
        "not enough free disk space in {}: need {}, only {} available",
        dest_dir.display(),
        pesto::progress::format_size(space.required),
        pesto::progress::format_size(space.available)
    );

    let (tx, rx) = penne::progress::channel();
    let progress_task = penne::ui::terminal::spawn_renderer(rx);

    let outcome = penne::download::download_queue(
        &queue,
        &config.server_tiers,
        &dest_dir,
        config.retries,
        Some(tx),
    )
    .await?;
    // `download_queue` now assembles every file internally as it completes,
    // so its own progress sender is the only copy left by the time it
    // returns — the channel closes on its own, and awaiting the renderer
    // here just waits for its last redraw to flush before the summary below
    // prints (avoiding any interleaving with the unbounded channel's
    // draining).
    progress_task.await.ok();

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

    let mut needs_repair = 0u32;
    for (name, result) in &outcome.assembled {
        match result {
            penne::assemble::AssembleOutcome::Complete { .. } => println!("  ok: {name}"),
            penne::assemble::AssembleOutcome::CompleteUnverified { .. } => {
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

    let synthetic_base = nzb
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("release");
    println!("checking for obfuscated/misnamed files...");
    let rename_report =
        penne::deobfuscate::run(&dest_dir, &queue, &outcome.assembled, synthetic_base).await?;
    if rename_report.renames.is_empty() {
        println!("  nothing to rename");
    }
    for r in &rename_report.renames {
        let label = match r.reason {
            penne::deobfuscate::RenameReason::Par2Volume => "par2 file",
            penne::deobfuscate::RenameReason::Par2Recovered => "recovered name (PAR2)",
            penne::deobfuscate::RenameReason::Guessed => "guessed name",
        };
        println!("  {label}: {} -> {}", r.old_name, r.new_name);
    }

    // The file names this release's PAR2 verification/repair is allowed to
    // touch under `dest_dir` — which can be shared across every `penne
    // download` run, so it may still hold leftover files from an unrelated,
    // previous release (see `penne::repair::find_par2_index`'s doc comment).
    // Starts from `outcome.assembled`'s keys (this run's own queue), then
    // applies the renames above so it reflects the names actually on disk
    // now, not the pre-deobfuscation ones.
    let known_files: std::collections::HashSet<String> = {
        let mut names: std::collections::HashSet<String> =
            outcome.assembled.keys().cloned().collect();
        for r in &rename_report.renames {
            names.remove(&r.old_name);
            names.insert(r.new_name.clone());
        }
        names
    };

    if needs_repair > 0 {
        let damaged = penne::health::damaged_bytes(&queue, &outcome.assembled);
        if let Some(health) = penne::health::evaluate(&dest_dir, damaged, &known_files)? {
            if !health.looks_repairable() {
                println!(
                    "  warning: {} missing/damaged, but only ~{} of PAR2 recovery data found \
                     — repair is unlikely to succeed",
                    pesto::progress::format_size(health.damaged_bytes),
                    pesto::progress::format_size(health.available_recovery_bytes)
                );
            }
        }
    }

    println!("checking PAR2 recovery data...");
    let (verify_tx, verify_rx) = penne::repair::channel();
    let verify_progress_task = penne::ui::verify::spawn_renderer(verify_rx);
    let repair_outcome = penne::repair::verify_and_repair(
        &dest_dir,
        &outcome.assembled,
        &known_files,
        Some(verify_tx),
    )
    .await?;
    // `true` here means a real, byte-exact verify pass ran (the quick-check
    // couldn't prove the release intact from already-known CRC-32s alone),
    // so at least one progress line was drawn during it already.
    let ran_full_verify = verify_progress_task.await.unwrap_or(false);
    match repair_outcome {
        penne::repair::RepairOutcome::Ok if !ran_full_verify => {
            println!("  quick-check passed from already-known checksums; full re-hash skipped")
        }
        penne::repair::RepairOutcome::Ok => println!("  PAR2: all files verified intact"),
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
            println!("  no PAR2 recovery data found; skipping verification");
            anyhow::ensure!(
                needs_repair == 0,
                "{needs_repair} file(s) incomplete or damaged, and no PAR2 recovery data was found to repair them"
            );
        }
    }

    println!("checking for archives to extract...");
    let password = password.as_deref().or(parsed.meta.password.as_deref());
    let extracted = penne::extract::extract_all(&dest_dir, password).await?;
    if extracted.is_empty() {
        println!("  nothing to extract");
    }
    for archive in &extracted {
        println!("  extracted: {} ({:?})", archive.base_name, archive.kind);
    }

    // Everything that needed fixing got fixed (we'd have bailed above
    // otherwise), so the cached article bodies kept for resume are no
    // longer needed.
    penne::cache::clear(&dest_dir)?;

    Ok(())
}

/// `penne download --stat`: verify every segment is still present on the
/// configured server(s) without downloading anything, and report per-file
/// completeness. Exits non-zero (via the returned `Err`) if anything is
/// missing, so it's scriptable ahead of a real download.
async fn check_availability(
    queue: &penne::queue::DownloadQueue,
    tiers: &[penne::config::ServerTier],
    retries: u32,
) -> Result<()> {
    let total_segments: usize = queue.files.iter().map(|f| f.segments.len()).sum();
    println!(
        "checking {} segment(s) across {} file(s)...",
        total_segments,
        queue.files.len()
    );

    let (tx, rx) = penne::check::channel();
    let progress_task = penne::ui::check::spawn_renderer(rx, total_segments as u32);

    let outcome = penne::check::check_queue(queue, tiers, retries, Some(tx)).await?;
    // `check_queue` owns the only sender clone, so it's already dropped by
    // the time it returns — the renderer's channel closes on its own and
    // this simply waits for its final redraw to flush.
    progress_task.await.ok();

    let mut incomplete_files = 0u32;
    for f in &outcome.files {
        if f.is_complete() {
            println!(
                "  complete: {} ({}/{} segments)",
                f.name, f.present_segments, f.total_segments
            );
        } else {
            incomplete_files += 1;
            println!(
                "  INCOMPLETE: {} ({}/{} segments)",
                f.name, f.present_segments, f.total_segments
            );
        }
    }
    for seg in &outcome.missing {
        println!("    missing: {} part {}", seg.file_name, seg.part);
    }

    let present_segments = total_segments as u32 - outcome.missing.len() as u32;
    let present_pct = if total_segments > 0 {
        present_segments as f64 / total_segments as f64 * 100.0
    } else {
        100.0
    };
    let complete_files = outcome.files.len() as u32 - incomplete_files;

    println!();
    println!("summary");
    println!("  articles present: {present_segments}/{total_segments} ({present_pct:.1}%)");
    println!(
        "  files complete:   {complete_files}/{}",
        outcome.files.len()
    );
    println!(
        "  data used:        {} (STAT only — no article data downloaded)",
        pesto::progress::format_size(outcome.bytes_used)
    );

    anyhow::ensure!(
        outcome.is_complete(),
        "{incomplete_files} file(s) have missing segments"
    );
    Ok(())
}
