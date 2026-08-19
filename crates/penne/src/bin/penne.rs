//! `penne` CLI: reads a `.nzb`, downloads it, assembles the result,
//! verifies/repairs it with PAR2 if recovery data was part of the release,
//! and extracts any archives (`.rar`/`.7z`/`.zip`) it finds. `--mode`
//! ([`ProcessingMode`]) picks how far down that pipeline a run goes,
//! mirroring `sabnzbd`'s per-category Download/+Repair/+Unpack/+Delete
//! processing levels.
//!
//! `info` and `download` are both functional end-to-end: fetch (Phase 2,
//! with per-segment retry/backoff, resume via [`penne::cache`], and
//! N-parallel-connections-per-server concurrency — Phases 8/9), yEnc decode
//! (Phase 3), file assembly (Phase 4), PAR2 verify/repair (Phase 6),
//! archive extraction (Phase 7), and post-extraction cleanup
//! ([`penne::cleanup`]).

use std::path::{Path, PathBuf};
use std::process;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use penne::check::CheckMethod;
use penne::config::ProcessingMode;

/// `penne download`'s exit codes — see [`Command::Download`]'s doc comment
/// for the user-facing description.
const EXIT_COMPLETE: i32 = 0;
const EXIT_REPAIRED: i32 = 1;
const EXIT_INCOMPLETE: i32 = 2;
const EXIT_FATAL: i32 = 3;

#[derive(Parser)]
#[command(
    name = "penne",
    version = penne::DISPLAY_VERSION,
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

    /// Increase log verbosity. Repeat for more detail:
    ///   `-v` = INFO (server selection, mode, PAR2/extract decisions),
    ///   `-vv` = DEBUG (NNTP commands and responses — credentials masked),
    ///   `-vvv` = TRACE (fine-grained timing and buffer events).
    /// Logs are written to stderr (or --log-file). `RUST_LOG` overrides the
    /// level when set. Matches `pesto`'s `-v`/`--verbose` convention.
    #[arg(short, long, action = clap::ArgAction::Count, global = true, value_name = "LEVEL")]
    verbose: u8,

    /// Redirect verbose log output to FILE instead of stderr. Has no effect
    /// without -v.
    #[arg(long, global = true, value_name = "FILE")]
    log_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Parse a `.nzb` and print file/segment/size counts.
    Info {
        /// Path to the `.nzb` file.
        nzb: PathBuf,
    },
    /// Download and assemble the contents of one or more `.nzb` files. Exits
    /// 0 if every file ended up complete with no repair needed, 1 if PAR2
    /// repaired something but the end result is complete, 2 if data is still
    /// missing or damaged (PAR2 couldn't fix it, no recovery data was
    /// available, or repair was skipped via `--mode download`), 3 on a fatal
    /// error (config, network, I/O). `--stat`'s own pass/fail (see below)
    /// surfaces as a fatal error too, since it never reaches the
    /// download/repair pipeline these codes describe.
    ///
    /// Multiple `.nzb` files download sequentially, sharing one `--config`/
    /// `--out-dir`/`--mode`/etc. for the whole batch; the overall exit code
    /// is the worst (highest) of any individual file's own code — one
    /// incomplete release in a batch of ten still needs to fail the run.
    /// Each release beyond the first downloads into its own subdirectory
    /// (named after its `.nzb` file's stem) under the shared destination, so
    /// same-named files across releases can never collide; a single `.nzb`
    /// keeps downloading straight into the destination, unchanged from
    /// before this flag accepted more than one path.
    Download {
        /// Path(s) to the `.nzb` file(s).
        #[arg(required = true)]
        nzb: Vec<PathBuf>,
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
        /// configured server(s) — no download, decode, PAR2, or
        /// extraction. Three methods, from cheapest-but-least-trustworthy
        /// to most expensive-but-certain: `stat` (the default when the
        /// flag is given with no value — RFC 3977 §6.2.4, a bare
        /// existence check against the server's index), `head` (RFC 3977
        /// §6.2.2 — still cheap, but reads from the same article storage
        /// `BODY` does, catching a provider whose `STAT` index has
        /// drifted out of sync with what it can actually deliver), or
        /// `body` (a full real fetch, discarded — maximum certainty, real
        /// bandwidth cost, no different from an actual download of the
        /// same segment).
        #[arg(long, value_enum, value_name = "METHOD")]
        stat: Option<Option<CheckMethod>>,
        /// Only meaningful with `--stat`: check `N` segment(s) of each file,
        /// spread evenly across it, instead of every segment in the
        /// release. Most useful with `--stat=body`, whose per-segment cost
        /// is a real article fetch — checking a whole large release that
        /// way often isn't worth it, but a small, protocol-normal sample
        /// (read to completion, connection closed cleanly — never an
        /// abandoned mid-transfer read, which real NNTP servers'
        /// anti-abuse systems tend to flag) still catches a provider whose
        /// article storage doesn't back up what it claims, wherever in the
        /// file that shows up — not just at the start. `0` is treated
        /// as `1` (sampling nothing would silently skip the file
        /// entirely, never useful).
        #[arg(long)]
        sample: Option<usize>,
        /// Use only the named [[servers]] entry for this run (matched by
        /// its `name` field in the config file), instead of every
        /// configured server. Repeat to pick more than one; they keep
        /// their relative order from the config file. Handy for a quick
        /// `--stat` against one particular provider without editing the
        /// config. Omit to use every configured server, as before this
        /// flag existed.
        #[arg(long = "server")]
        server: Vec<String>,
        /// How much post-processing to do after fetching, mirroring
        /// `sabnzbd`'s per-category processing levels. Each level does
        /// everything the previous one does, plus one more step:
        /// `download` (fetch + assemble only) -> `repair` (+ PAR2
        /// verify/repair) -> `unpack` (+ extract archives) -> `delete`
        /// (+ delete the compressed volumes and PAR2 recovery data once
        /// extraction succeeds, leaving only the release's other files).
        /// Defaults to the config file's `mode`, or `unpack` if that's
        /// unset too.
        #[arg(long, value_enum)]
        mode: Option<ProcessingMode>,
        /// Suppress the live progress panel; only status/result lines print.
        /// Matches `pesto`'s `-q`/`--quiet` convention — handy for tmux/screen
        /// sessions or when output is redirected to a log file.
        #[arg(long, short)]
        quiet: bool,
    },
    /// Check article availability across one or more `.nzb` files without
    /// downloading. Exits 0 if all articles are present, 1 if any are
    /// confirmed missing (a server returned a definitive "not present"),
    /// 2 on fatal error, 3 if inconclusive (no confirmed-missing article,
    /// but at least one segment never got a real answer from any
    /// configured server — a connection failure, not a `430`).
    Check {
        /// One or more `.nzb` files to check.
        #[arg(required = true)]
        nzb: Vec<PathBuf>,
        /// Which NNTP command to use: `stat` (default, cheapest), `head`
        /// (reads from article storage, catches stale STAT indices), or
        /// `body` (full fetch, discarded — maximum certainty).
        #[arg(long, value_enum, default_value = "stat")]
        method: CheckMethod,
        /// Check only N segments of each file (spread evenly across it)
        /// instead of all.
        #[arg(long)]
        sample: Option<usize>,
        /// STAT commands pipelined per connection (default: 128).
        #[arg(long, default_value = "128")]
        pipeline_depth: usize,
        /// Machine-readable JSON output.
        #[arg(long)]
        json: bool,
        /// Suppress progress bar, print only the final summary.
        #[arg(long, short)]
        quiet: bool,
        /// Use only the named server(s) from the config file.
        #[arg(long = "server")]
        server: Vec<String>,
        /// Check each configured server independently instead of using them
        /// as failover backups. Outputs a separate result for each server.
        #[arg(long)]
        independent_servers: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    pesto::logging::init(cli.verbose, cli.log_file.as_deref(), None)?;

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
            sample,
            server,
            mode,
            quiet,
        }) => {
            let stat = stat.map(|inner| inner.unwrap_or_default());
            let multi = nzb.len() > 1;
            let mut worst_exit_code = EXIT_COMPLETE;
            for (i, path) in nzb.iter().enumerate() {
                if multi {
                    println!("=== [{}/{}] {} ===", i + 1, nzb.len(), path.display());
                }
                // Beyond the first release, each gets its own subdirectory
                // (named after its .nzb's stem) under the shared destination
                // so same-named files across releases can never collide. A
                // single .nzb keeps the old flat destination unchanged.
                let subdir = multi.then(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("release")
                        .to_string()
                });
                let exit_code = download(
                    path,
                    out_dir.clone(),
                    cli.config.clone().flatten(),
                    password.clone(),
                    stat,
                    sample,
                    &server,
                    mode,
                    quiet,
                    subdir.as_deref(),
                )
                .await
                .unwrap_or_else(|e| {
                    eprintln!("error: {e:#}");
                    EXIT_FATAL
                });
                worst_exit_code = worst_exit_code.max(exit_code);
            }
            process::exit(worst_exit_code);
        }
        Some(Command::Check {
            nzb,
            method,
            sample,
            pipeline_depth,
            json,
            quiet,
            server,
            independent_servers,
        }) => {
            let exit_code = check(
                &nzb,
                method,
                sample,
                pipeline_depth,
                json,
                quiet,
                cli.config.flatten(),
                &server,
                independent_servers,
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("error: {e:#}");
                2
            });
            process::exit(exit_code);
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

#[allow(clippy::too_many_arguments)]
async fn download(
    nzb: &Path,
    out_dir: Option<PathBuf>,
    config_path: Option<PathBuf>,
    password: Option<String>,
    stat: Option<CheckMethod>,
    sample: Option<usize>,
    server_names: &[String],
    cli_mode: Option<ProcessingMode>,
    quiet: bool,
    subdir: Option<&str>,
) -> Result<i32> {
    anyhow::ensure!(
        stat.is_some() || sample.is_none(),
        "--sample only makes sense with --stat; a real download always fetches every segment"
    );
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
    let config = penne::config::RawConfig::parse(&config_toml)?
        .select(server_names)?
        .resolve()?;
    anyhow::ensure!(
        !config.server_tiers.is_empty(),
        "no [[servers]] configured in {}",
        config_path.display()
    );
    let mode = cli_mode.unwrap_or(config.mode);

    if let Some(method) = stat {
        // `--stat` never reaches the download/repair pipeline, so it doesn't
        // participate in the complete/repaired/incomplete distinction below —
        // it either confirms availability (0) or fails outright (surfaced as
        // an `Err`, exit EXIT_FATAL), per its own doc comment.
        let Some(per_file) = sample else {
            return check_availability(&queue, &config.server_tiers, method, config.retries)
                .await
                .map(|()| EXIT_COMPLETE);
        };
        let sampled = penne::queue::sample(&queue, per_file);
        let full_total: usize = queue.files.iter().map(|f| f.segments.len()).sum();
        let sampled_total: usize = sampled.files.iter().map(|f| f.segments.len()).sum();
        println!(
            "sampling {sampled_total} of {full_total} segment(s) ({} per file, {} file(s))...",
            per_file.max(1),
            sampled.files.len()
        );
        return check_availability(&sampled, &config.server_tiers, method, config.retries)
            .await
            .map(|()| EXIT_COMPLETE);
    }

    let dest_dir = out_dir.unwrap_or(config.download_dir);
    let dest_dir = match subdir {
        Some(name) => dest_dir.join(name),
        None => dest_dir,
    };

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
    let progress_task = if !quiet {
        Some(penne::ui::terminal::spawn_renderer(rx))
    } else {
        drop(rx);
        None
    };

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
    if let Some(task) = progress_task {
        task.await.ok();
    }

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

    let repair_note = if mode >= ProcessingMode::Repair {
        "will attempt PAR2 repair"
    } else {
        "PAR2 repair skipped, --mode download"
    };
    let mut needs_repair = 0u32;
    for (name, result) in &outcome.assembled {
        match result {
            penne::assemble::AssembleOutcome::Complete { .. } => println!("  ok: {name}"),
            penne::assemble::AssembleOutcome::CompleteUnverified { .. } => {
                println!("  ok (unverified): {name}")
            }
            penne::assemble::AssembleOutcome::ChecksumMismatch { .. } => {
                needs_repair += 1;
                println!("  damaged ({repair_note}): {name} ({result:?})");
            }
            penne::assemble::AssembleOutcome::Incomplete { .. } => {
                needs_repair += 1;
                println!("  incomplete ({repair_note}): {name} ({result:?})");
            }
        }
    }
    // Provisional: raised to EXIT_REPAIRED below if PAR2 fixes something, or
    // returned early as EXIT_INCOMPLETE if it can't. Stays EXIT_INCOMPLETE
    // as-is when repair is skipped entirely (`--mode download` with
    // `needs_repair > 0`) — the data really is incomplete on disk even
    // though the user chose not to attempt a fix this run.
    let mut exit_code = if needs_repair == 0 {
        EXIT_COMPLETE
    } else {
        EXIT_INCOMPLETE
    };

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

    if mode >= ProcessingMode::Repair {
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
        // `true` here means a real, byte-exact verify pass ran (the
        // quick-check couldn't prove the release intact from
        // already-known CRC-32s alone), so at least one progress line was
        // drawn during it already.
        let ran_full_verify = verify_progress_task.await.unwrap_or(false);
        match repair_outcome {
            penne::repair::RepairOutcome::Ok if !ran_full_verify => {
                println!("  quick-check passed from already-known checksums; full re-hash skipped");
                exit_code = EXIT_COMPLETE;
            }
            penne::repair::RepairOutcome::Ok => {
                println!("  PAR2: all files verified intact");
                exit_code = EXIT_COMPLETE;
            }
            penne::repair::RepairOutcome::Repaired(plan) => {
                for f in &plan.repaired_files {
                    println!(
                        "  PAR2 repaired: {} ({} slice(s))",
                        f.name, f.slices_repaired
                    );
                }
                exit_code = EXIT_REPAIRED;
            }
            penne::repair::RepairOutcome::NotRepairable(report) => {
                eprintln!(
                    "error: {} damaged slice(s) exceed available PAR2 recovery data ({} block(s)); download is incomplete",
                    report.total_bad_slices(),
                    report.available_recovery_blocks
                );
                // Bails out here (skipping extraction/cleanup/cache-clear
                // below) exactly like the old `anyhow::bail!` did — the
                // difference is this is a known, reported outcome (exit
                // EXIT_INCOMPLETE), not a generic fatal `Err`.
                return Ok(EXIT_INCOMPLETE);
            }
            penne::repair::RepairOutcome::NoRecoveryData => {
                println!("  no PAR2 recovery data found; skipping verification");
                if needs_repair > 0 {
                    eprintln!(
                        "error: {needs_repair} file(s) incomplete or damaged, and no PAR2 recovery data was found to repair them"
                    );
                    return Ok(EXIT_INCOMPLETE);
                }
                exit_code = EXIT_COMPLETE;
            }
        }
    } else if needs_repair > 0 {
        println!(
            "  warning: {needs_repair} file(s) incomplete or damaged; rerun with --mode repair \
             (or higher) to fix them"
        );
    }

    if mode >= ProcessingMode::Unpack {
        println!("checking for archives to extract...");
        let password = password.as_deref().or(parsed.meta.password.as_deref());
        let extracted = penne::extract::extract_all(&dest_dir, password).await?;
        if extracted.is_empty() {
            println!("  nothing to extract");
        }
        for archive in &extracted {
            println!("  extracted: {} ({:?})", archive.base_name, archive.kind);
        }
    } else {
        let mode_name = match mode {
            ProcessingMode::Download => "download",
            ProcessingMode::Repair => "repair",
            ProcessingMode::Unpack | ProcessingMode::Delete => {
                unreachable!("mode < Unpack means Download or Repair")
            }
        };
        println!("skipping archive extraction, --mode {mode_name}");
    }

    if mode >= ProcessingMode::Delete {
        println!("cleaning up archives and PAR2 recovery data...");
        let deleted = penne::cleanup::purge_archives_and_par2(&dest_dir, &known_files).await?;
        if deleted.is_empty() {
            println!("  nothing to clean up");
        }
        for name in &deleted {
            println!("  deleted: {name}");
        }
    }

    // Below `--mode repair`, nothing here verified whether the fetch was
    // actually complete — if it wasn't, the resume cache must survive so a
    // later, higher `--mode` run can still avoid refetching. At `--mode
    // repair` or above, reaching this point without having already bailed
    // out means everything that needed fixing got fixed, so the cache is
    // safe to drop.
    if mode >= ProcessingMode::Repair || needs_repair == 0 {
        penne::cache::clear(&dest_dir)?;
    }

    Ok(exit_code)
}

/// `penne download --stat`: verify every segment is still present on the
/// configured server(s) without downloading anything, and report per-file
/// completeness. Exits non-zero (via the returned `Err`) if anything is
/// missing, so it's scriptable ahead of a real download.
async fn check_availability(
    queue: &penne::queue::DownloadQueue,
    tiers: &[penne::config::ServerTier],
    method: CheckMethod,
    retries: u32,
) -> Result<()> {
    let total_segments: usize = queue.files.iter().map(|f| f.segments.len()).sum();
    println!(
        "checking {} segment(s) across {} file(s) via {method}...",
        total_segments,
        queue.files.len()
    );

    let (tx, rx) = penne::check::channel();
    let progress_task = penne::ui::check::spawn_renderer(rx, total_segments as u32);

    let config = penne::check::CheckConfig::new(method, retries);
    let outcome = penne::check::check_queue(queue, tiers, &config, Some(tx)).await?;
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
    for seg in &outcome.unreachable {
        println!(
            "    unreachable: {} part {} (no server gave a definitive answer)",
            seg.file_name, seg.part
        );
    }

    let present_pct = if outcome.total_checked > 0 {
        outcome.total_present as f64 / outcome.total_checked as f64 * 100.0
    } else {
        100.0
    };
    let complete_files = outcome.files.len() as u32 - incomplete_files;

    println!();
    println!("summary");
    println!(
        "  articles present: {}/{} ({present_pct:.1}%)",
        outcome.total_present, outcome.total_checked
    );
    println!(
        "  files complete:   {complete_files}/{}",
        outcome.files.len()
    );
    let data_used_note = match method {
        CheckMethod::Stat => "STAT only — no article data downloaded",
        CheckMethod::Head => "HEAD only — headers only, no article body downloaded",
        CheckMethod::Body => {
            "full BODY fetch — real article data downloaded, nothing written to disk"
        }
    };
    println!(
        "  data used:        {} ({data_used_note})",
        pesto::progress::format_size(outcome.bytes_used)
    );
    println!(
        "  elapsed:          {:.1}s ({:.0} articles/sec)",
        outcome.elapsed.as_secs_f64(),
        outcome.articles_per_second()
    );

    anyhow::ensure!(
        outcome.is_complete(),
        "{incomplete_files} file(s) incomplete: {} confirmed-missing segment(s), \
         {} unreachable segment(s) (no server gave a definitive answer)",
        outcome.missing.len(),
        outcome.unreachable.len()
    );
    Ok(())
}

/// `penne check`: first-class article availability checker with JSON output,
/// exit codes (0=all present, 1=confirmed missing, 2=fatal error,
/// 3=inconclusive — no confirmed-missing segment, but at least one
/// unreachable), multi-NZB support, configurable pipeline depth, and quiet
/// mode.
#[allow(clippy::too_many_arguments)]
async fn check(
    nzb_paths: &[PathBuf],
    method: CheckMethod,
    sample: Option<usize>,
    pipeline_depth: usize,
    json: bool,
    quiet: bool,
    config_path: Option<PathBuf>,
    server_names: &[String],
    independent_servers: bool,
) -> Result<i32> {
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
            default
        }
    };
    let config_toml = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = penne::config::RawConfig::parse(&config_toml)?
        .select(server_names)?
        .resolve()?;
    anyhow::ensure!(
        !config.server_tiers.is_empty(),
        "no [[servers]] configured in {}",
        config_path.display()
    );

    let check_config = penne::check::CheckConfig {
        method,
        pipeline_depth,
        retries: config.retries,
    };

    let flat_servers: Vec<penne::config::ServerTier> = config
        .server_tiers
        .iter()
        .flat_map(|tier| tier.members.iter().cloned())
        .map(penne::config::ServerTier::solo)
        .collect();

    let mut any_missing = false;
    let mut any_unreachable = false;

    let mut queues = Vec::new();
    let mut nzb_names = Vec::new();
    let mut total_segments = 0;

    for nzb_path in nzb_paths {
        let parsed = penne::nzb::load(nzb_path)?;
        let mut queue = penne::queue::build(&parsed);
        if let Some(per_file) = sample {
            queue = penne::queue::sample(&queue, per_file);
        }
        total_segments += queue.files.iter().map(|f| f.segments.len()).sum::<usize>();
        queues.push(queue);
        nzb_names.push(
            nzb_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string(),
        );
    }

    let tiers_to_run: Vec<Vec<penne::config::ServerTier>> = if independent_servers {
        flat_servers.iter().map(|t| vec![t.clone()]).collect()
    } else {
        vec![config.server_tiers.clone()]
    };

    // Gated on `!quiet` only, not `!json`: the live bar and this banner both
    // write to stderr (see ui/check.rs), never stdout, so they don't corrupt
    // `--json`'s NDJSON output on stdout — a caller piping stdout to a file
    // (e.g. curupira's remote-check.sh) still gets to watch progress on the
    // terminal instead of sitting with zero feedback through a long batch.
    if !quiet {
        if independent_servers {
            eprintln!(
                "checking {} segment(s) across {} NZB(s) via {} on {} server(s) concurrently...",
                total_segments,
                queues.len(),
                method,
                flat_servers.len()
            );
        } else {
            eprintln!(
                "checking {} segment(s) across {} NZB(s) via {}...",
                total_segments,
                queues.len(),
                method
            );
        }
    }

    let total_work = total_segments * tiers_to_run.len();
    let (tx, rx) = penne::check::channel();
    let progress_task = if !quiet {
        Some(penne::ui::check::spawn_renderer(rx, total_work as u32))
    } else {
        drop(rx);
        None
    };

    let (outcome_tx, mut outcome_rx) = tokio::sync::mpsc::unbounded_channel::<(
        Option<String>,
        usize,
        penne::check::CheckOutcome,
    )>();
    let nzb_names_clone = nzb_names.clone();
    let is_json = json;
    let method_str = method.to_string();
    let retries = check_config.retries;
    // Hostnames tried, in priority order, when servers are combined into a
    // single check rather than run `--independent-servers` (which already
    // reports its own server per line via `server_label`).
    let aggregated_servers: Vec<String> = config
        .server_tiers
        .iter()
        .flat_map(|tier| tier.members.iter().map(|m| m.host.clone()))
        .collect();

    let print_task = tokio::spawn(async move {
        if is_json {
            while let Some((server_label, q_idx, outcome)) = outcome_rx.recv().await {
                let nzb_name = &nzb_names_clone[q_idx];
                let mut json_val = serde_json::json!({
                    "nzb": nzb_name,
                    // Wall-clock time this outcome was resolved, not when the
                    // check started — with multiple NZBs/servers finishing at
                    // different times, a single run-start timestamp would be
                    // misleading for the later lines.
                    "checked_at": chrono::Utc::now().to_rfc3339(),
                    "method": method_str,
                    "retries": retries,
                    // `complete` requires every segment be confirmed present —
                    // false if any is confirmed missing OR merely unreachable.
                    // Check `conclusive` before trusting `missing`/`missing_pct`
                    // as a final verdict: if `conclusive` is false, at least one
                    // segment never got a real answer from any server, and
                    // treating that as confirmed absence is the false positive
                    // this field split exists to prevent.
                    "complete": outcome.is_complete(),
                    "conclusive": outcome.is_conclusive(),
                    "total_articles": outcome.total_checked,
                    "present": outcome.total_present,
                    "missing": outcome.missing_count(),
                    "missing_pct": if outcome.total_checked > 0 {
                        outcome.missing_count() as f64 / outcome.total_checked as f64 * 100.0
                    } else {
                        0.0
                    },
                    "unreachable": outcome.unreachable_count(),
                    "unreachable_pct": if outcome.total_checked > 0 {
                        outcome.unreachable_count() as f64 / outcome.total_checked as f64 * 100.0
                    } else {
                        0.0
                    },
                    "files": outcome.files,
                    "missing_articles": outcome.missing,
                    "unreachable_articles": outcome.unreachable,
                    "bytes_used": outcome.bytes_used,
                    "elapsed_secs": outcome.elapsed.as_secs_f64(),
                    "articles_per_second": outcome.articles_per_second(),
                });
                let obj = json_val.as_object_mut().unwrap();
                if let Some(ref s) = server_label {
                    obj.insert("server".to_string(), serde_json::Value::String(s.clone()));
                } else {
                    obj.insert(
                        "servers".to_string(),
                        serde_json::to_value(&aggregated_servers).unwrap(),
                    );
                }
                if let Some(n) = sample {
                    obj.insert("sample_size".to_string(), serde_json::Value::from(n));
                }
                println!("{}", serde_json::to_string(&json_val).unwrap());
            }
        } else {
            // Drain the channel so it doesn't block senders
            while outcome_rx.recv().await.is_some() {}
        }
    });

    let mut join_set = tokio::task::JoinSet::new();

    for tiers_batch in tiers_to_run {
        let server_label = if independent_servers {
            let s = &tiers_batch[0].members[0];
            Some(s.host.clone())
        } else {
            None
        };

        let qs = queues.clone();
        let tb = tiers_batch.clone();
        let c = check_config.clone();
        let txc = tx.clone();

        let out_tx = outcome_tx.clone();
        join_set.spawn(async move {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

            let forward_task = {
                let sl = server_label.clone();
                tokio::spawn(async move {
                    while let Some((q_idx, outcome)) = rx.recv().await {
                        let _ = out_tx.send((sl.clone(), q_idx, outcome));
                    }
                })
            };

            let outcomes = penne::check::check_nzbs(&qs, &tb, &c, Some(txc), Some(tx)).await;
            forward_task.await.ok();
            (server_label, outcomes)
        });
    }

    drop(tx);
    drop(outcome_tx);

    let mut tier_outcomes = Vec::new();
    while let Some(res) = join_set.join_next().await {
        tier_outcomes.push(res.expect("task panicked"));
    }

    if let Some(task) = progress_task {
        task.await.ok();
    }
    print_task.await.ok();

    tier_outcomes.sort_by(|a, b| a.0.cmp(&b.0));

    for (server_label, outcomes_res) in tier_outcomes {
        let outcomes = outcomes_res?;
        for (i, outcome) in outcomes.into_iter().enumerate() {
            let nzb_name = &nzb_names[i];

            if !json {
                if queues.len() > 1 {
                    println!("\n[{}/{}] {}", i + 1, queues.len(), nzb_name);
                }

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
                if !quiet {
                    for seg in &outcome.missing {
                        println!("    missing: {} part {}", seg.file_name, seg.part);
                    }
                    for seg in &outcome.unreachable {
                        println!(
                            "    unreachable: {} part {} (no server gave a definitive answer)",
                            seg.file_name, seg.part
                        );
                    }
                }

                let present_pct = if outcome.total_checked > 0 {
                    outcome.total_present as f64 / outcome.total_checked as f64 * 100.0
                } else {
                    100.0
                };
                let complete_files = outcome.files.len() as u32 - incomplete_files;

                println!();
                if let Some(ref s) = server_label {
                    println!("summary ({s})");
                } else {
                    println!("summary");
                }
                println!(
                    "  articles present: {}/{} ({:.1}%)",
                    outcome.total_present, outcome.total_checked, present_pct
                );
                if !outcome.unreachable.is_empty() {
                    let unreachable_pct = outcome.unreachable.len() as f64
                        / outcome.total_checked.max(1) as f64
                        * 100.0;
                    println!(
                        "  unreachable:       {} ({:.1}%) — no server gave a definitive answer; \
                         not counted as missing, but this check is inconclusive",
                        outcome.unreachable.len(),
                        unreachable_pct
                    );
                }
                println!(
                    "  files complete:   {}/{}",
                    complete_files,
                    outcome.files.len()
                );
                let data_used_note = match method {
                    CheckMethod::Stat => "STAT only — no article data downloaded",
                    CheckMethod::Head => "HEAD only — headers only, no article body downloaded",
                    CheckMethod::Body => {
                        "full BODY fetch — real article data downloaded, nothing written to disk"
                    }
                };
                println!(
                    "  data used:        {} ({data_used_note})",
                    pesto::progress::format_size(outcome.bytes_used)
                );
                println!(
                    "  elapsed:          {:.1}s ({:.0} articles/sec)",
                    outcome.elapsed.as_secs_f64(),
                    outcome.articles_per_second()
                );
            }

            if !outcome.missing.is_empty() {
                any_missing = true;
            }
            if !outcome.unreachable.is_empty() {
                any_unreachable = true;
            }
        }
    }

    // A confirmed-missing article always wins over "merely inconclusive" —
    // once we know for certain the release is broken, that's more
    // actionable than "we couldn't fully confirm it".
    Ok(if any_missing {
        1
    } else if any_unreachable {
        3
    } else {
        0
    })
}
