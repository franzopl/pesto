//! Orchestration for one complete upload entry.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use pesto::config::ObfuscateMode;
use pesto::nntp::pool::ConnectionBroker;
use tracing::{error, info};

use super::super::batch::apply_ext_filter;
use super::super::hooks::{run_pre_hook, run_pre_hooks_dir, HookEnv};
use super::{
    plan_upload_paths, resolve_entry_password, resume_flags_string, PhaseTimings, UploadParams,
    UploadResult,
};

/// Run one complete upload: expand `entry_paths`, compress, post, write NZB.
///
/// Returns the posted segments so the caller can build a consolidated season NZB.
///
/// `keep_compress_temp`: when this entry compresses its input, the archive
/// normally lives only in a per-entry temp dir that's deleted before this
/// function returns — fine for a standalone upload, since nothing needs the
/// archive bytes afterward. A `--season` batch does: `post_season_par2_volumes`
/// runs after every episode has posted, and must compute the season's global
/// PAR2 over the *actual posted bytes* (the archive), not the original
/// episode file, or the resulting PAR2 set describes data that was never put
/// on the wire. Setting this to `true` skips that inline cleanup and reports
/// the temp dir back via `UploadResult::compress_temp_dir` instead, so the
/// caller can defer deletion until after it's done reading `posted_paths`.
pub(crate) async fn run_single_upload(
    params: &UploadParams,
    entry_paths: &[PathBuf],
    entry_label: &str,
    cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    forced_password: Option<&str>,
    keep_compress_temp: bool,
    broker: Option<Arc<ConnectionBroker>>,
) -> Result<UploadResult> {
    let config = &params.config;
    // Resolved once, used for the pre-upload summary, the archive itself,
    // and the .nzb/history/hook metadata alike — so all of them agree on
    // the exact password that ends up protecting this entry's archive.
    let effective_password: Option<String> = resolve_entry_password(
        forced_password,
        config.compress_password.as_deref(),
        params.archive_password_raw.as_deref(),
    );
    let upload_start = std::time::Instant::now();
    let mut timings = PhaseTimings::default();

    let mut inputs = pesto::walk::expand_inputs(entry_paths)?;
    apply_ext_filter(&mut inputs, &params.ext_filter, entry_label)?;
    let (_file_count, _folder_count, total_bytes) = upload_summary(&inputs);
    // Snapshot the pre-compression file list: `inputs` gets overwritten below
    // with the single archive file when --compress is active, but hooks still
    // need the original filenames (e.g. to detect a video file by extension
    // for thumbnail generation) regardless of what was actually posted.
    let original_inputs = inputs.clone();

    // Run pre-hook(s) before anything else (before compression, PAR2, or NNTP).
    // Non-zero exit from any hook aborts the upload immediately.
    // --no-hooks suppresses only the pre-hooks/ directory; --pre-hook always runs
    // (matching the post-hook behaviour established in the PR that fixed no_hooks).
    if !config.dry_run {
        let input_paths_str = inputs
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let pre_obfuscate = match config.obfuscate {
            ObfuscateMode::None => "none",
            ObfuscateMode::Full => "full",
            ObfuscateMode::Light => "light",
            ObfuscateMode::FullShared => "full-shared",
            ObfuscateMode::Article => "article",
        };
        let pre_groups_str = config.groups.join(":");
        let pre_tags_str = config.nzb_tags.join(" ");
        // No upload has happened yet, so report every server that will get a
        // connection quota (config.host plus extra_servers) rather than just
        // the primary — with [[servers]] all of them start receiving
        // connections immediately, unlike `groups`, where only one is
        // eventually chosen at random.
        let pre_servers_str = config
            .all_servers()
            .map(|s| s.host)
            .collect::<Vec<_>>()
            .join(":");
        let pre_env = HookEnv {
            nzb_path: None,
            nfo_path: None,
            name: entry_label,
            total_bytes,
            input_paths: &input_paths_str,
            group: config.groups.first().map(String::as_str),
            groups: &pre_groups_str,
            password: None,
            server: pre_servers_str.split(':').next().unwrap_or(&config.host),
            servers: &pre_servers_str,
            category: config.nzb_category.as_deref(),
            nzb_title: config.nzb_title.as_deref(),
            obfuscate: pre_obfuscate,
            par2: config.par2,
            tags: &pre_tags_str,
            tmdb_id: config.tmdb_id.as_deref(),
            imdb_id: config.imdb_id.as_deref(),
            tvdb_id: config.tvdb_id.as_deref(),
            mal_id: config.mal_id.as_deref(),
            incomplete: false,
        };

        // Explicit --pre-hook always runs (not suppressed by --no-hooks).
        for cmd in &config.pre_hooks {
            run_pre_hook(cmd, &pre_env)?;
        }

        // Directory scripts are suppressed by --no-hooks.
        if !config.no_hooks {
            if let Some(pre_hooks_dir) = pesto::config::config_dir().map(|d| d.join("pre-hooks")) {
                run_pre_hooks_dir(&pre_hooks_dir, &pre_env)?;
            }
        }
    }

    if !params.json_mode && !params.renderer_opts.quiet && std::io::stderr().is_terminal() {
        pesto::progress::print_tree(&inputs);
        let compress_fmt = config.compress_format.as_deref().or_else(|| {
            if effective_password.is_some() {
                Some("7z")
            } else {
                None
            }
        });
        pesto::progress::print_upload_flags(&pesto::progress::UploadFlags {
            obfuscate: match config.obfuscate {
                ObfuscateMode::None => "none",
                ObfuscateMode::Full => "full",
                ObfuscateMode::Light => "light",
                ObfuscateMode::FullShared => "full-shared",
                ObfuscateMode::Article => "article",
            },
            compress: compress_fmt,
            password: effective_password.as_deref(),
            par2: config.par2,
            resume: config.resume,
            check: config.check,
        });
    }

    let (progress_tx, renderer) = if params.json_mode {
        pesto::progress::spawn_json_emitter()
    } else {
        pesto::ui::terminal::spawn_renderer_with(params.renderer_opts.clone())
    };

    let upload_paths = plan_upload_paths(params, entry_paths, &inputs);
    let nzb_out_path = upload_paths.nzb_out_path;
    let nzb_user_dest = upload_paths.nzb_user_dest;
    let resume_path = upload_paths.resume_path;

    let compression = super::compression::run(
        config,
        inputs,
        entry_label,
        resume_path.as_deref(),
        effective_password.as_deref(),
        params.archive_password_raw.as_deref() == Some(""),
        &progress_tx,
    )
    .await?;
    timings.compress_ms = compression.elapsed_ms;
    let compress_temp_dir = compression.temp_dir;
    let light_compressed_prefix = compression.light_prefix;
    let inputs = compression.inputs;

    // Captured now, after `inputs` has taken its final (possibly compressed)
    // form and before posting: the exact set of files that are about to be
    // put on the wire, for a `--season` batch's later global PAR2 step.
    let posted_paths: Vec<PathBuf> = inputs.iter().map(|f| f.path.clone()).collect();

    let t_post = std::time::Instant::now();
    let outcome = pesto::poster::post_files_inner_with_release_prefix(
        config,
        &inputs,
        Some(progress_tx),
        resume_path.as_deref(),
        cancel.cloned(),
        Some(entry_label),
        broker,
        None,
        light_compressed_prefix.as_deref(),
    )
    .await?;
    let _ = renderer.await;
    timings.post_ms = Some(t_post.elapsed().as_millis());

    // `post_files_with_progress_and_cancel` already retried in-run POST
    // failures (repost_failed_tasks) and ran the streaming STAT check +
    // repost internally, concurrently with the upload. `outcome.still_missing`
    // is MissingConfirmed (430); `outcome.inconclusive` is a failed check
    // path, not a confirmed gap. Cancel drain is Inconclusive, not missing.
    let cancelled = outcome.cancelled || cancel.is_some_and(|f| f.load(Ordering::Relaxed));
    let check_missing: Vec<String> = if cancelled {
        Vec::new()
    } else {
        outcome.still_missing.clone()
    };
    let check_inconclusive: Vec<String> = if cancelled {
        Vec::new()
    } else {
        outcome.inconclusive.clone()
    };

    if !params.json_mode && config.par2_only {
        if cancelled {
            println!("PAR2 generation interrupted.");
        } else {
            println!("PAR2 generation complete.");
        }
    }

    if cancelled {
        // `outcome.cancelled` is set both by a real user cancellation and by a
        // producer error (bad PAR2 geometry, a memory-budget check, file I/O,
        // …) — see `PostOutcome::failure_reason`. Printing the same generic
        // "interrupted" text for both left a run that actually failed with no
        // indication of why, and the same file would then fail identically on
        // every retry with no clue that retrying wouldn't help (issue #57).
        if let Some(reason) = &outcome.failure_reason {
            eprintln!("upload failed: {reason}");
        } else if config.par2_only {
            eprintln!("interrupted — stopped before finishing PAR2 generation");
        } else {
            eprintln!("interrupted — upload incomplete");
        }
    }
    if !outcome.failures.is_empty() {
        eprintln!("{} segment(s) failed:", outcome.failures.len());
        for failure in &outcome.failures {
            eprintln!("  - {failure}");
        }
    }
    // `check_missing` is already final: `post_files_with_progress_and_cancel`
    // ran the streaming STAT check and every repost attempt internally,
    // concurrently with the upload, so there is no separate repost round to
    // drive here anymore.
    if !cancelled
        && config.check
        && !config.dry_run
        && !config.par2_only
        && !outcome.segments.is_empty()
    {
        if check_missing.is_empty() {
            // Success is already reported: the renderer's final summary shows
            // "all verified" (TTY) and `draw_plain`'s last line carries the
            // check tally (non-TTY/-v). A second "check: all N verified" line
            // here would just duplicate it.
        } else {
            eprintln!(
                "check: {} article(s) still missing after every repost attempt:",
                check_missing.len()
            );
            for id in &check_missing {
                eprintln!("  - {id}");
            }
            error!(
                count = check_missing.len(),
                ids = ?check_missing,
                "check: articles still missing after every repost attempt"
            );
        }
        if !check_inconclusive.is_empty() {
            eprintln!(
                "check: {} article(s) inconclusive (check path failed — not a confirmed gap):",
                check_inconclusive.len()
            );
            for id in &check_inconclusive {
                eprintln!("  - {id}");
            }
            error!(
                count = check_inconclusive.len(),
                ids = ?check_inconclusive,
                "check: articles inconclusive (check path failed — not a confirmed gap)"
            );
        }
    }

    // If segments still failed after retry, refuse to write the NZB — it
    // would be incomplete. The resume state already has all successfully
    // posted segments so the user can continue with --resume.
    let has_post_failures =
        !outcome.failed_tasks.is_empty() && !config.dry_run && !config.par2_only;
    // STAT 430-exhausted after every --check-post-retries round.
    // `--allow-incomplete-nzb` opts back into publishing only this kind of
    // gap; POST failures and Inconclusive always block.
    let has_confirmed_missing = !check_missing.is_empty() && !config.dry_run && !config.par2_only;
    let has_inconclusive = !check_inconclusive.is_empty() && !config.dry_run && !config.par2_only;
    let has_unrecoverable_failures = pesto::poster::nzb_write_decision(
        has_post_failures,
        has_confirmed_missing,
        has_inconclusive,
        config.allow_incomplete_nzb,
    ) == pesto::poster::NzbWriteDecision::Refuse;
    let files_str = || {
        entry_paths
            .iter()
            .map(|p| format!("\"{}\"", p.display()))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let resume_flags_str = || resume_flags_string(config);
    if has_post_failures {
        let n = outcome.failed_tasks.len();
        eprintln!();
        eprintln!("error: {n} segment(s) could not be posted after all retries.");
        eprintln!("The NZB will NOT be written — the upload is incomplete.");
        // Resume state is tracked for every run (not just ones started with
        // --resume) and persisted whenever a run ends incomplete like this
        // one — see `post_files_with_progress_and_cancel`'s final
        // persist-or-delete decision — so the segments that did succeed are
        // always recoverable here, regardless of whether --resume was
        // originally passed.
        if let Some(ref state_path) = resume_path {
            eprintln!();
            eprintln!("The successfully posted segments have been saved to:");
            eprintln!("  {}", state_path.display());
            eprintln!();
            eprintln!("To retry the missing segments and finish the upload, run:");
            eprintln!("  pesto {} --resume {}", files_str(), resume_flags_str());
        }
        eprintln!();
    }
    if has_confirmed_missing {
        let n = check_missing.len();
        eprintln!();
        if config.allow_incomplete_nzb {
            eprintln!(
                "warning: {n} article(s) still missing on the server after every repost \
                 attempt, including one final automatic recovery pass when the miss count \
                 was small enough."
            );
            eprintln!("Publishing anyway — --allow-incomplete-nzb was set.");
        } else {
            eprintln!(
                "error: {n} article(s) still missing on the server after every repost \
                 attempt, including one final automatic recovery pass when the miss count \
                 was small enough."
            );
            eprintln!(
                "The NZB will NOT be written — pass --allow-incomplete-nzb to publish anyway \
                 (e.g. relying on PAR2 recovery)."
            );
            // Same reasoning as the has_post_failures branch above: resume
            // state is always tracked and gets persisted here regardless of
            // whether --resume was passed to this run.
            if let Some(ref state_path) = resume_path {
                eprintln!();
                eprintln!(
                    "Or retry just the missing article(s) — the segments already \
                     confirmed present have been saved to:"
                );
                eprintln!("  {}", state_path.display());
                eprintln!("  pesto {} --resume {}", files_str(), resume_flags_str());
            }
        }
        eprintln!();
    }
    if has_inconclusive {
        let n = check_inconclusive.len();
        eprintln!();
        eprintln!("error: {n} article(s) inconclusive (check path failed — not a confirmed gap).");
        eprintln!(
            "The NZB will NOT be written — --allow-incomplete-nzb does not apply to \
             an unverified check path."
        );
        if let Some(ref state_path) = resume_path {
            eprintln!();
            eprintln!(
                "Retry with --resume --check to re-STAT the same Message-IDs \
                 (no second POST):"
            );
            eprintln!("  {}", state_path.display());
            eprintln!("  pesto {} --resume {}", files_str(), resume_flags_str());
        }
        eprintln!();
    }

    let nzb_reported_path = super::artifacts::write(super::artifacts::ArtifactRequest {
        params,
        nzb_out_path,
        nzb_user_dest,
        has_unrecoverable_failures,
        effective_password: effective_password.as_deref(),
        outcome: &outcome,
        entry_label,
        total_bytes,
        duration_secs: upload_start.elapsed().as_secs_f64(),
    })
    .await?;

    super::completion::run(super::completion::CompletionRequest {
        params,
        entry_paths,
        entry_label,
        original_inputs: &original_inputs,
        effective_password: effective_password.as_deref(),
        outcome: &outcome,
        nzb_reported_path: nzb_reported_path.as_deref(),
        cancelled,
        has_post_failures,
        has_confirmed_missing,
        has_inconclusive,
        has_unrecoverable_failures,
        total_bytes,
    })
    .await?;

    // Cleanup temp dirs. When `keep_compress_temp` is set, the caller still
    // needs `posted_paths` on disk (a `--season` batch's global PAR2 step
    // reads them after every episode has finished) — leave the archive in
    // place and let the caller remove it once done.
    let compress_temp_dir = if keep_compress_temp {
        compress_temp_dir
    } else {
        if let Some(dir) = &compress_temp_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        None
    };
    // Only now — after the --check repost pass and the end-of-run failed-task
    // retry above have both had every chance to re-read a PAR2 file's bytes —
    // is it safe to remove the PAR2 temp dir. See `par2_temp_dir`'s doc
    // comment for why this used to happen too early.
    if !config.par2_only {
        outcome.cleanup_par2_temp_dir().await;
    }

    // 26g — per-phase timing summary (only when -v is active)
    if tracing::enabled!(tracing::Level::INFO) {
        let total_ms = upload_start.elapsed().as_millis();
        let mut parts = Vec::<String>::new();
        if let Some(ms) = timings.compress_ms {
            parts.push(format!("compress={ms}ms"));
        }
        if let Some(ms) = timings.post_ms {
            parts.push(format!("post={ms}ms"));
        }
        info!(
            total_ms,
            phases = %parts.join(" "),
            "upload timing summary"
        );
    }

    // Apply cleanup only if upload succeeded completely (no failures/cancellation).
    let no_failures = outcome.failures.is_empty()
        && check_missing.is_empty()
        && check_inconclusive.is_empty()
        && !has_unrecoverable_failures;
    let should_cleanup = !cancelled && no_failures;

    if should_cleanup {
        for entry_path in entry_paths {
            if let Err(e) = params.cleanup_mode.cleanup(entry_path) {
                eprintln!("cleanup: {e:#}");
            }
        }
    }

    Ok(UploadResult {
        segments: outcome.segments,
        groups: outcome.groups,
        cancelled,
        had_failures: !outcome.failures.is_empty()
            || !check_missing.is_empty()
            || !check_inconclusive.is_empty()
            || has_unrecoverable_failures,
        inconclusive: check_inconclusive,
        total_bytes,
        nzb_path: nzb_reported_path,
        posted_paths,
        compress_temp_dir,
    })
}

/// Aggregate the upload as `(file count, subfolder count, total bytes)`.
fn upload_summary(inputs: &[pesto::walk::InputFile]) -> (usize, usize, u64) {
    let mut subfolders = std::collections::BTreeSet::new();
    let mut bytes = 0u64;
    for input in inputs {
        let components: Vec<&str> = input.name.split('/').collect();
        let mut prefix = String::new();
        for component in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            if prefix.contains('/') {
                subfolders.insert(prefix.clone());
            }
        }
        if let Ok(metadata) = std::fs::metadata(&input.path) {
            bytes += metadata.len();
        }
    }
    (inputs.len(), subfolders.len(), bytes)
}
