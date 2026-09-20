use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use penne::check::CheckMethod;
use penne::config::ProcessingMode;

use super::command::{EXIT_COMPLETE, EXIT_INCOMPLETE, EXIT_REPAIRED};

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
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
