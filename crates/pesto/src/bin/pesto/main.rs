//! `pesto` — fast, lean Usenet poster.
//!
//! Parses the CLI, resolves the configuration, posts the given files to Usenet
//! and writes an `.nzb` file describing the result.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use pesto::config::{self, Config, FileConfig, ObfuscateMode};
use pesto::logging;
use pesto::nntp::pool::ConnectionBroker;
use pesto::nzb::NzbMeta;
use tracing::{error, info};

mod batch;
mod cleanup;
mod cli;
mod hooks;
mod output;
mod season;
mod upload;
mod watch;

use batch::{apply_ext_filter, derive_season_nzb_path, release_label, run_batch};
use cleanup::CleanupMode;
use cli::Cli;
use hooks::{run_all_hooks, run_pre_hook, run_pre_hooks_dir, HookEnv};
use output::{nzb_archive_path, resolve_nzb_dest};
use upload::{
    plan_upload_paths, resolve_entry_password, resume_flags_string, PhaseTimings, UploadParams,
    UploadResult,
};
use watch::{run_watch, WatchBatchOpts};

/// Tracks this process's exact live-heap byte count (see
/// [`pesto::memory::alloc`]), for comparison against `VmSize`/`RLIMIT_AS` in
/// `--memory-report`. Declared here — in the binary, not the `pesto` library
/// — because `#[global_allocator]` is a whole-binary choice; `upapasta`,
/// `penne` and `sugo` link `pesto` as a library and are unaffected by it.
#[global_allocator]
static ALLOC: pesto::memory::alloc::CountingAlloc = pesto::memory::alloc::CountingAlloc::new();

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
async fn run_single_upload(
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

    let compression = upload::compression::run(
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

    // Write NZB.
    // The canonical copy goes to ~/.config/pesto/nzb/TIMESTAMP_stem.nzb.
    // If the user specified a destination (--out or nzb_dir), a hardlink (or
    // copy when cross-device) is placed there so re-uploads never collide.
    let out: Option<PathBuf> = if let Some(stem) = nzb_out_path {
        Some(nzb_archive_path(&stem).await)
    } else {
        None
    };

    // nzb_reported_path: the path shown to the user and passed to hooks/history.
    // It is the user-dest (hardlink) when set, otherwise the archive copy.
    let mut nzb_reported_path: Option<PathBuf> = nzb_user_dest.clone().or_else(|| out.clone());

    let _nzb_xml: Option<String> = if let Some(out) = &out {
        if !config.par2_only {
            if has_unrecoverable_failures {
                eprintln!("skipping nzb output — upload incomplete");
                nzb_reported_path = None;
                None
            } else if outcome.segments.is_empty() {
                eprintln!("no segments posted — skipping nzb output");
                nzb_reported_path = None;
                None
            } else {
                let mut nzb_tags = config.nzb_tags.clone();
                add_obfuscation_tag(&mut nzb_tags, &config.obfuscate);
                let nzb_meta = NzbMeta {
                    name: config.nzb_title.clone(),
                    password: config
                        .nzb_password
                        .clone()
                        .or_else(|| effective_password.clone()),
                    category: config.nzb_category.clone(),
                    tmdb_id: config.tmdb_id.clone(),
                    imdb_id: config.imdb_id.clone(),
                    tvdb_id: config.tvdb_id.as_deref().map(|id| {
                        format!(
                            "{}/{id}",
                            config
                                .tvdb_kind
                                .unwrap_or(pesto::nzb::TvdbKind::Series)
                                .as_str()
                        )
                    }),
                    mal_id: config.mal_id.clone(),
                    tags: nzb_tags,
                };
                let xml = pesto::nzb::generate(
                    &outcome.groups,
                    &outcome.segments,
                    &nzb_meta,
                    config.obfuscate,
                );
                tokio::fs::write(out, &xml)
                    .await
                    .with_context(|| format!("writing nzb file `{}`", out.display()))?;

                // Place a hardlink (or copy) at the user-requested destination,
                // respecting the nzb_conflict policy.
                if let Some(dest) = &nzb_user_dest {
                    if let Some(parent) = dest.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    let effective_dest = resolve_nzb_dest(dest, config.nzb_conflict).await?;
                    if std::fs::hard_link(out, &effective_dest).is_err() {
                        std::fs::copy(out, &effective_dest).with_context(|| {
                            format!("copying nzb to `{}`", effective_dest.display())
                        })?;
                    }
                    nzb_reported_path = Some(effective_dest);
                }

                let reported = nzb_reported_path.as_deref().unwrap_or(out);
                if params.json_mode {
                    let path_esc = reported
                        .display()
                        .to_string()
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"");
                    println!(r#"{{"type":"nzb_written","path":"{path_esc}"}}"#);
                } else {
                    println!("wrote nzb: {}", reported.display());
                }

                // Append to shared history catalog.
                if params.write_history && !config.par2_only && !config.dry_run {
                    let obf_name = if config.obfuscate != pesto::config::ObfuscateMode::None {
                        Some(entry_label)
                    } else {
                        None
                    };
                    let par2_str;
                    let par2_pct = if config.par2 > 0 {
                        par2_str = format!("{}%", config.par2);
                        Some(par2_str.as_str())
                    } else {
                        None
                    };
                    // The server(s) that actually accepted an article this
                    // run (`outcome.servers`), not just the statically
                    // configured primary — a multi-server (failover) config
                    // commonly uses every configured server at once.
                    let history_servers_str = outcome.servers.join(", ");
                    let wire_subjects_vec = pesto::nzb::wire_subjects(&outcome.segments);
                    pesto::history::record_upload(
                        &pesto::history::UploadRecord {
                            name: entry_label,
                            obfuscated_name: obf_name,
                            password: effective_password.as_deref(),
                            total_bytes,
                            // The group actually posted to (`pick_post_group`
                            // chose one at random from `config.groups`), not
                            // the configured list's static first entry.
                            group: outcome.groups.first().map(String::as_str),
                            server: (!history_servers_str.is_empty())
                                .then_some(history_servers_str.as_str()),
                            par2_redundancy: par2_pct,
                            duration_secs: upload_start.elapsed().as_secs_f64(),
                            nzb_path: Some(&reported.display().to_string()),
                            subject: config.nzb_title.as_deref().or(Some(entry_label)),
                            wire_subjects: &wire_subjects_vec,
                        },
                        config.history_dir.as_deref(),
                    );
                }

                Some(xml)
            }
        } else {
            None
        }
    } else {
        None
    };

    // Send completion notifications.
    let notify_enabled = config.notify.unwrap_or(true)
        && (config.notify_webhook.is_some() || config.notify_ntfy.is_some());
    if notify_enabled && !config.par2_only && !config.dry_run && !cancelled {
        // Reflects true completeness, independent of --allow-incomplete-nzb —
        // the notification should say "not fully ok" even when the user
        // chose to publish anyway.
        let had_failures = !outcome.failures.is_empty()
            || has_post_failures
            || has_confirmed_missing
            || has_inconclusive;
        pesto::notify::send_all(&pesto::notify::NotifyConfig {
            webhook_url: config.notify_webhook.as_deref(),
            ntfy_topic: config.notify_ntfy.as_deref(),
            name: entry_label,
            total_bytes,
            group: outcome.groups.first().map(String::as_str),
            category: config.nzb_category.as_deref(),
            ok: !had_failures,
        })
        .await;
    }

    // Generate .nfo as a local artifact only when the upload actually
    // succeeded. Writing it on failure leaves an orphan `.nfo` in the input
    // directory (no nzb_reported_path → fallback next to the source files),
    // which `--resume --each` would later pick up as a standalone release.
    let upload_ok = !cancelled && outcome.failures.is_empty() && !has_unrecoverable_failures;
    let nfo_path: Option<PathBuf> = if config.nfo && upload_ok && !config.par2_only {
        let base = nzb_reported_path
            .as_ref()
            .map(|p| p.with_extension("nfo"))
            .or_else(|| {
                entry_paths
                    .first()
                    .and_then(|p| p.parent())
                    .map(|d| d.join(format!("{entry_label}.nfo")))
            });
        if let Some(ref nfo_out) = base {
            // `nfo::generate` blocks on `bdinfo`/`mediainfo`, which can take
            // a while on a large Blu-ray disc — long enough that, with no
            // output in between, it looks like the process hung. Run it on
            // a blocking-pool thread and print a heartbeat every 10s so
            // there's always something on screen while it works.
            if pesto::nfo::looks_like_bluray(entry_paths) {
                println!(
                    "generating nfo (running bdinfo — this can take a while on large Blu-ray discs)..."
                );
            } else {
                println!("generating nfo...");
            }
            pesto::memory::set_phase(pesto::memory::Phase::Nfo);
            let nfo_paths = entry_paths.to_vec();
            let nfo_handle = tokio::task::spawn_blocking(move || pesto::nfo::generate(&nfo_paths));
            tokio::pin!(nfo_handle);
            let nfo_content = loop {
                tokio::select! {
                    res = &mut nfo_handle => break res.context("nfo generation task panicked")?,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                        println!("... still generating nfo, please wait");
                    }
                }
            };
            match nfo_content {
                Some(content) => match pesto::nfo::write(
                    nfo_out,
                    &format!("{}{content}", nfo_metadata_header(config)),
                ) {
                    Ok(()) => {
                        println!("wrote nfo:  {}", nfo_out.display());
                        Some(nfo_out.clone())
                    }
                    Err(e) => {
                        eprintln!("nfo write failed: {e}");
                        None
                    }
                },
                None => {
                    eprintln!("nfo: no content generated for the given paths");
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // Run post-upload hooks only when the upload actually succeeded.
    if upload_ok && !config.par2_only && !config.dry_run {
        // Use `original_inputs`, not `inputs`: when --compress is active
        // `inputs` was replaced with the single compressed archive, which
        // would otherwise hide every original filename (and its extension)
        // from post-upload hooks — see the `original_inputs` snapshot above.
        let post_input_paths = original_inputs
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let post_obfuscate = match config.obfuscate {
            ObfuscateMode::None => "none",
            ObfuscateMode::Full => "full",
            ObfuscateMode::Light => "light",
            ObfuscateMode::FullShared => "full-shared",
            ObfuscateMode::Article => "article",
        };
        // PESTO_GROUP/PESTO_GROUPS report the group(s) actually posted to
        // (`outcome.groups`, chosen at random by `pick_post_group` from the
        // full configured list), not the static configured list itself —
        // this is a post-upload hook, so the real destination is known.
        let post_groups_str = outcome.groups.join(":");
        // Same reasoning for PESTO_SERVER/PESTO_SERVERS: report the
        // server(s) that actually accepted an article (`outcome.servers`),
        // not just the statically configured primary.
        let post_servers_str = outcome.servers.join(":");
        let post_tags_str = config.nzb_tags.join(" ");
        let hook_env = HookEnv {
            nzb_path: nzb_reported_path.as_deref(),
            nfo_path: nfo_path.as_deref(),
            name: entry_label,
            total_bytes,
            input_paths: &post_input_paths,
            group: outcome.groups.first().map(String::as_str),
            groups: &post_groups_str,
            password: effective_password.as_deref(),
            server: post_servers_str.split(':').next().unwrap_or(&config.host),
            servers: &post_servers_str,
            category: config.nzb_category.as_deref(),
            nzb_title: config.nzb_title.as_deref(),
            obfuscate: post_obfuscate,
            par2: config.par2,
            tags: &post_tags_str,
            tmdb_id: config.tmdb_id.as_deref(),
            imdb_id: config.imdb_id.as_deref(),
            tvdb_id: config.tvdb_id.as_deref(),
            mal_id: config.mal_id.as_deref(),
            incomplete: has_confirmed_missing,
        };

        run_all_hooks(config, &hook_env);
    }

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

/// Build the `IMDb:`/`TMDb:`/`TVDB:`/`MAL:` header block prepended to a
/// generated `.nfo` when any of `--tmdb`, `--imdb-id`, `--tvdb-id` or
/// `--mal-id` were set. Returns an empty string when none is set.
fn nfo_metadata_header(config: &Config) -> String {
    let mut header = String::new();
    if let Some(imdb_id) = &config.imdb_id {
        header.push_str(&format!("IMDb : https://www.imdb.com/title/{imdb_id}/\n"));
    }
    if let Some(tmdb_id) = &config.tmdb_id {
        header.push_str(&format!("TMDb : https://www.themoviedb.org/{tmdb_id}\n"));
    }
    if let Some(tvdb_id) = &config.tvdb_id {
        // The dereferrer link resolves by ID alone, without needing the
        // title's slug — but the path segment must still match the media
        // kind (movie vs. series), unlike a plain numeric ID.
        let kind = config
            .tvdb_kind
            .unwrap_or(pesto::nzb::TvdbKind::Series)
            .as_str();
        header.push_str(&format!(
            "TVDB : https://thetvdb.com/dereferrer/{kind}/{tvdb_id}\n"
        ));
    }
    if let Some(mal_id) = &config.mal_id {
        header.push_str(&format!("MAL  : https://myanimelist.net/anime/{mal_id}\n"));
    }
    if !header.is_empty() {
        header.push('\n');
    }
    header
}

// ── NZB metadata helpers ──────────────────────────────────────────────────────

/// Add obfuscation mode tag to NZB metadata tags.
/// This helps indexers understand what mode was used during posting.
fn add_obfuscation_tag(tags: &mut Vec<String>, obfuscate: &ObfuscateMode) {
    match obfuscate {
        ObfuscateMode::None => {}
        ObfuscateMode::Full => {
            tags.push("obfuscated:full".to_string());
        }
        ObfuscateMode::Light => {
            tags.push("obfuscated:light".to_string());
        }
        ObfuscateMode::Article => {
            tags.push("obfuscated:article".to_string());
        }
        ObfuscateMode::FullShared => {
            tags.push("obfuscated:full-shared".to_string());
        }
    }
}

// ── merge-season ─────────────────────────────────────────────────────────────

/// Group all `.nzb` files in `dir` by season, merge each group into one
/// combined NZB, and write it beside the source files.
fn run_merge_season(dir: &Path, display_name: Option<&str>, nzb_tags: Vec<String>) -> Result<()> {
    use std::collections::BTreeMap;

    anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());

    // Collect .nzb files, sorted so episodes come out in order.
    let mut nzb_files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("nzb"))
        .collect();
    nzb_files.sort();

    anyhow::ensure!(
        !nzb_files.is_empty(),
        "no .nzb files found in {}",
        dir.display()
    );

    // Group files by season key.  A season key is the show name plus the
    // season number extracted from the filename, e.g. "Batwheels.S02".
    // Files with no recognisable season marker fall into a catch-all group
    // named after the directory.
    let fallback_key = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "season".into());

    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in &nzb_files {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = season_key(&stem).unwrap_or_else(|| fallback_key.clone());
        groups.entry(key).or_default().push(path.clone());
    }

    for (key, files) in &groups {
        // Skip if only one file in the group — nothing to merge.
        // (Single-file "seasons" are already complete NZBs.)
        if files.len() < 2 {
            eprintln!("skipping {key}: only one NZB in group");
            continue;
        }

        let output_path = dir.join(format!("{key}.nzb"));

        // Don't include the output file itself if it already exists in `files`.
        let sources: Vec<&PathBuf> = files
            .iter()
            .filter(|p| p.as_path() != output_path.as_path())
            .collect();

        eprintln!(
            "\nmerging {} episodes into {}",
            sources.len(),
            output_path.display()
        );

        let mut combined_segments: Vec<pesto::poster::PostedSegment> = Vec::new();
        let mut poster = String::new();
        let mut all_groups: Vec<String> = Vec::new();

        for src in &sources {
            let content = std::fs::read_to_string(src)
                .with_context(|| format!("reading {}", src.display()))?;
            let parsed = pesto::nzb::parse(&content)
                .with_context(|| format!("parsing {}", src.display()))?;

            let ep_name = src
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| src.display().to_string());
            let file_count = parsed
                .segments
                .iter()
                .map(|s| &s.file_name)
                .collect::<std::collections::HashSet<_>>()
                .len();
            let seg_count = parsed.segments.len();
            eprintln!("  + {ep_name}  ({file_count} file(s), {seg_count} segment(s))");

            if poster.is_empty() {
                poster = parsed.poster;
            }
            for g in parsed.groups {
                if !all_groups.contains(&g) {
                    all_groups.push(g);
                }
            }
            combined_segments.extend(parsed.segments);
        }

        combined_segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));

        let meta = pesto::nzb::NzbMeta {
            name: display_name
                .map(str::to_string)
                .or_else(|| Some(key.clone())),
            password: None,
            category: None,
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            mal_id: None,
            tags: nzb_tags.clone(),
        };
        // Segments here come from `nzb::parse`, which always leaves
        // `wire_name` empty (see its doc comment) — there is no live wire
        // identity to mirror when merging already-generated `.nzb` files,
        // so the obfuscate mode passed here is moot; `None` just keeps this
        // call explicit about that.
        let xml = pesto::nzb::generate(
            &all_groups,
            &combined_segments,
            &meta,
            pesto::config::ObfuscateMode::None,
        );

        std::fs::write(&output_path, &xml)
            .with_context(|| format!("writing {}", output_path.display()))?;

        eprintln!(
            "wrote {} ({} total segments)",
            output_path.display(),
            combined_segments.len()
        );
    }

    Ok(())
}

/// Extract a season group key from an NZB stem.
///
/// `Batwheels.S02E32-E33.1080p.NF.WEB-DL` → `Batwheels.S02`
/// `Show.Name.s01e01.720p`                  → `Show.Name.S01`
/// `Random.File`                            → `None`
fn season_key(stem: &str) -> Option<String> {
    let lower = stem.to_lowercase();
    let bytes = lower.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b's' {
            // Require at least one digit after 's'.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j == i + 1 {
                continue; // no digits after 's'
            }
            // Require 'e' followed by at least one digit.
            if j < bytes.len()
                && bytes[j] == b'e'
                && j + 1 < bytes.len()
                && bytes[j + 1].is_ascii_digit()
            {
                // stem[..j] covers everything up to 'e', including 'SXX'.
                // Reconstruct with original case up to the 's', then uppercase season.
                let prefix = &stem[..i];
                let season_num = &stem[i + 1..j]; // digits only
                return Some(format!(
                    "{prefix}S{:0>2}",
                    season_num.parse::<u32>().unwrap_or(0)
                ));
            }
        }
    }
    None
}

/// Append a one-line structured summary to the session log file.
///
/// Written after the upload completes so it is always the last line, making
/// `tail -1` a reliable way to check the outcome of any upload.
fn write_session_summary(
    path: &Path,
    label: &str,
    cancelled: bool,
    had_failures: bool,
    total_bytes: u64,
    nzb_path: Option<&Path>,
) {
    use std::io::Write;

    let status = if cancelled {
        "cancelled"
    } else if had_failures {
        "failed"
    } else {
        "ok"
    };

    let total_mb = total_bytes as f64 / 1_048_576.0;
    let nzb = nzb_path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("-");

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = format!(
        "{now}  summary  status={status}  label=\"{label}\"  bytes={total_mb:.1}MiB  nzb={nzb}\n"
    );

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Entry point.
///
/// Deliberately *not* `#[tokio::main]`: two things have to happen before the
/// runtime spawns its first thread, and the attribute leaves no room for
/// either.
///
/// 1. `tune_allocator()` must run before any thread exists — on glibc,
///    malloc's per-core arenas are created lazily on first allocation from a
///    new thread and can never be reclaimed afterwards.
/// 2. The runtime itself must be built with bounded thread counts and stack
///    sizes. `#[tokio::main]`'s defaults (`ncores` workers, 2 MiB stacks) are
///    the largest avoidable consumer of address space on a many-core seedbox:
///    measured over a full `--par2-only --threads 128` run, bounding them
///    takes peak address space from 803.5 MiB to 443.3 MiB. Against a typical
///    seedbox `ulimit -v` that headroom is the difference between finishing a
///    100 GiB post and aborting mid-encode. See [`pesto::memory`].
fn main() -> Result<()> {
    pesto::memory::tune_allocator();
    let tuning = pesto::memory::ThreadTuning::detect();
    let runtime = tuning
        .build_runtime()
        .context("building the tokio runtime")?;
    let result = runtime.block_on(run(tuning));
    // Logged from here rather than at the end of `run` so it covers the error
    // paths too. It does not cover the `std::process::exit` calls on Ctrl-C —
    // those bypass every unwind and destructor by design.
    info!("memory: {} (exit)", pesto::memory::peak_summary());
    if pesto::memory::report_enabled() {
        let ceiling = pesto::memory::Ceiling::discover(pesto::memory::explicit_memory_limit());
        println!("{}", pesto::memory::report_summary(&ceiling));
    }
    result
}

async fn run(tuning: pesto::memory::ThreadTuning) -> Result<()> {
    let mut cli = Cli::parse();

    // `pesto --config` with no value: launch the interactive setup wizard.
    if matches!(cli.config, Some(None)) {
        return pesto::ui::wizard::run();
    }

    if cli.update {
        return pesto::update::run().await;
    }

    // Handle `-` (stdin) in the file list.
    // Read all of stdin into a temp file and replace the `-` path with it.
    // Only one `-` is allowed per invocation; combining with --each/--season
    // is not supported (PAR2 and compression require a real file on disk).
    let _stdin_tempfile: Option<tempfile::NamedTempFile>;
    if cli.files.iter().any(|p| p.as_os_str() == "-") {
        if cli.files.iter().filter(|p| p.as_os_str() == "-").count() > 1 {
            anyhow::bail!("stdin (`-`) may only appear once in the file list");
        }
        if cli.each || cli.season {
            anyhow::bail!("stdin (`-`) cannot be combined with --each or --season");
        }
        let stdin_name = cli
            .stdin_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("--stdin-name is required when reading from stdin (`-`)")
            })?;

        use std::io::Read;
        if std::io::stdin().is_terminal() {
            anyhow::bail!("stdin is a terminal; pipe data into pesto or use a file instead of `-`");
        }

        // Read stdin into a named temp file so poster.rs can seek and stat it.
        let mut tmp = tempfile::Builder::new()
            .prefix("pesto_stdin_")
            .tempfile()
            .context("creating stdin temp file")?;
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .context("reading from stdin")?;
        std::io::Write::write_all(&mut tmp, &buf).context("writing stdin to temp file")?;
        let tmp_path = tmp.path().to_path_buf();
        // Keep the temp file alive until the upload is done.
        _stdin_tempfile = Some(tmp);

        // Replace `-` with the temp path and set the published name via a
        // special sentinel that run_single_upload will recognise.
        for p in &mut cli.files {
            if p.as_os_str() == "-" {
                *p = tmp_path.clone();
            }
        }
        // Store the desired name in cli.stdin_name; run_single_upload will
        // use it when building InputFile from the temp path.
        // We rename the file itself so expand_inputs picks up the right base name.
        // Easiest: just rename the temp file to have the desired name as its last component.
        let named_tmp_dir = tmp_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        let named_path = named_tmp_dir.join(stdin_name);
        // Only rename if the paths differ (avoid overwriting if name matches).
        if named_path != tmp_path {
            std::fs::hard_link(&tmp_path, &named_path)
                .or_else(|_| std::fs::copy(&tmp_path, &named_path).map(|_| ()))
                .context("naming stdin temp file")?;
            for p in &mut cli.files {
                if *p == tmp_path {
                    *p = named_path.clone();
                }
            }
        }
    } else {
        _stdin_tempfile = None;
    }

    // --out names a single fixed file; combined with --watch --season, every
    // distinct folder detected over time would silently clobber the same
    // path. Point the user at --nzb-dir instead, which names each season NZB
    // after its folder.
    anyhow::ensure!(
        !(cli.watch.is_some() && cli.season && cli.out.is_some()),
        "--out cannot be combined with --watch --season (each detected folder needs its own \
         season NZB name); use --nzb-dir instead"
    );

    // --merge-season: offline NZB merge, no server connection needed.
    if let Some(ref dir) = cli.merge_season {
        // No upload here, so no session log — just honour -v/--log-file.
        logging::init(cli.verbose, cli.log_file.as_deref(), None)?;
        let nzb_tags = if !cli.nzb_tag.is_empty() {
            cli.nzb_tag.clone()
        } else {
            let fc = match &cli.config {
                Some(Some(path)) => FileConfig::load(path).ok(),
                _ => config::default_config_path()
                    .filter(|p| p.exists())
                    .and_then(|p| FileConfig::load(&p).ok()),
            };
            fc.map(|c| c.output.nzb_tags).unwrap_or_default()
        };
        let nzb_title = cli.nzb_title.as_deref().or_else(|| {
            cli.nzb_name.as_deref().inspect(|_| {
                eprintln!(
                    "warning: --nzb-name is deprecated, use --nzb-title instead; \
                     --nzb-name will stop being accepted in a future release"
                );
            })
        });
        return run_merge_season(dir, nzb_title, nzb_tags);
    }

    // `pesto` with nothing to post and no --watch: show the orientation screen.
    let has_work = !cli.files.is_empty() || cli.watch.is_some();
    if !has_work {
        print_welcome();
        return Ok(());
    }

    print_header();
    if let Some(notice) = pesto::update::check_notice().await {
        eprintln!("{notice}");
    }

    // Resolve config file.
    let (file_config, nzb_default) = match &cli.config {
        Some(Some(path)) => (FileConfig::load(path)?, None),
        _ => {
            let default_path = config::default_config_path();
            match default_path.as_deref().filter(|p| p.exists()) {
                Some(path) => {
                    eprintln!("using config: {}", path.display());
                    let fc = FileConfig::load(path)?;
                    let nzb = fc.output.nzb.clone();
                    (fc, nzb)
                }
                // Nothing found at the OS-standard location: say exactly where
                // pesto looked, so a config placed at the wrong path (e.g.
                // ~/.config on Windows, which pesto never checks — see #43)
                // doesn't look like it's being silently ignored.
                None => {
                    match &default_path {
                        Some(path) => eprintln!(
                            "no config found at {} — using CLI flags/built-in defaults only. \
                             Run `pesto --config` to create one there.",
                            path.display()
                        ),
                        None => eprintln!(
                            "no config directory could be determined for this OS — using CLI \
                             flags/built-in defaults only."
                        ),
                    }
                    (FileConfig::default(), None)
                }
            }
        }
    };
    let nzb_default = nzb_default.or_else(|| file_config.output.nzb.clone());
    // Read before `file_config` is consumed by `Config::resolve`.
    let session_log_enabled = !cli.no_session_log && file_config.output.session_log.unwrap_or(true);
    let config = Arc::new(Config::resolve(file_config, cli.overrides())?);
    let json_mode = cli.output_format.trim().eq_ignore_ascii_case("json");

    // Initialise logging now that the history directory is known. The verbose
    // (`-v`) output goes to stderr or --log-file as before; in parallel, unless
    // disabled, every upload also writes a DEBUG log to `<history_dir>/logs/`
    // so it can be analysed afterwards without re-running with -vv.
    let session_log = if session_log_enabled {
        let name = cli
            .files
            .iter()
            .find(|p| p.as_os_str() != "-")
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .or_else(|| cli.watch.as_ref().map(|_| "watch".to_string()))
            .unwrap_or_else(|| "pesto".to_string());
        pesto::history::session_log_path(config.history_dir.as_deref(), &name, 50)
    } else {
        None
    };
    logging::init(cli.verbose, cli.log_file.as_deref(), session_log.as_deref())?;
    logging::log_system_info();
    // Logged unconditionally at INFO (not gated on -vv): when a run dies to
    // `handle_alloc_error` there is no unwind and no further output, so the
    // last line already in the log is the only evidence of how much address
    // space was left. See `pesto::memory`.
    info!(
        worker_threads = tuning.worker_threads,
        max_blocking_threads = tuning.max_blocking_threads,
        thread_stack_kib = tuning.thread_stack_size / 1024,
        "memory: {} (startup)",
        pesto::memory::footprint_summary(),
    );
    // Started after logging is up so `--memory-trace` output has somewhere to
    // go. Peak-and-stage tracking (and pressure-level logging) is always on;
    // only the per-sample trace line is gated on the flag. `Ceiling` is cheap
    // to (re)compute — see `memory::Ceiling::discover` — so it's read fresh
    // here rather than threaded through from anywhere earlier. It does need
    // `config.memory_limit` (the resolved global `--memory-limit`, if any)
    // so the sampler's pressure percentages and `--memory-report`'s
    // breakdown agree with what `producer` actually enforced.
    pesto::memory::set_report_enabled(cli.memory_report);
    pesto::memory::set_explicit_memory_limit(config.memory_limit);
    pesto::memory::start_sampler(
        cli.memory_trace,
        pesto::memory::Ceiling::discover(config.memory_limit),
    );
    if let Some(p) = &session_log {
        tracing::debug!(path = %p.display(), "session log");
    }

    // Fall back to the append-only plain renderer whenever verbose logs share
    // stderr with the panel — at *any* -v level, not just -vv: an INFO-level
    // `-v` run also writes connection/pool lines to stderr, which the panel's
    // cursor-movement redraws would shred (and be shredded by). If the user
    // redirected logs to a file with --log-file the panel can run safely.
    let logs_to_stderr = cli.verbose >= 1 && cli.log_file.is_none();

    // Validate mutually exclusive flags.
    anyhow::ensure!(
        !(cli.cleanup && cli.cleanup_to.is_some()),
        "--cleanup and --cleanup-to are mutually exclusive"
    );

    let cleanup_mode = if cli.cleanup {
        CleanupMode::Delete
    } else if let Some(dir) = cli.cleanup_to.clone() {
        CleanupMode::MoveTo(dir)
    } else {
        CleanupMode::Leave
    };

    let params = Arc::new(UploadParams {
        config: Arc::clone(&config),
        archive_password_raw: cli.archive_password.clone(),
        nzb_default: nzb_default.map(|s| s.to_string()),
        json_mode,
        out: cli.out.clone(),
        write_history: config.history,
        renderer_opts: pesto::progress::RendererOptions {
            quiet: cli.quiet || config.quiet,
            bell: cli.bell || config.bell,
            plain: logs_to_stderr,
        },
        ext_filter: cli
            .ext
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect(),
        cleanup_mode,
    });

    // Unified cancellation flag: one signal listener for the whole process.
    let cancel = Arc::new(AtomicBool::new(false));
    pesto::cancel::spawn_listener(cancel.clone());

    // ── --watch mode ──────────────────────────────────────────────────────────
    if let Some(watch_dir) = &cli.watch {
        let any_cancelled = run_watch(
            params,
            watch_dir,
            cli.watch_done.as_deref(),
            cli.watch_interval,
            cli.jobs,
            WatchBatchOpts {
                each: cli.each,
                season: cli.season,
                explicit_out: cli.out.clone(),
            },
            cancel,
        )
        .await?;
        if any_cancelled {
            std::process::exit(130);
        }
        return Ok(());
    }

    // ── --each / --season batch mode ─────────────────────────────────────────
    let batch_mode = cli.each || cli.season;
    if batch_mode {
        // For --season, derive the consolidated NZB path from the first directory arg.
        let season_nzb: Option<PathBuf> = if cli.season {
            cli.out.clone().or_else(|| {
                cli.files
                    .iter()
                    .find(|p| std::fs::metadata(p).map(|md| md.is_dir()).unwrap_or(false))
                    .map(|entry| {
                        derive_season_nzb_path(None, entry, params.config.nzb_dir.as_deref())
                    })
            })
        } else {
            None
        };

        let (_, any_cancelled, any_failures) =
            run_batch(params, &cli.files, cli.jobs, season_nzb, cancel).await?;

        if any_cancelled {
            std::process::exit(130);
        }
        if any_failures {
            std::process::exit(1);
        }
        return Ok(());
    }

    // ── Single upload (normal mode) ───────────────────────────────────────────
    // Same label helper as `--season`/`--each` (no is_dir/stat on the
    // async executor). See `release_label`.
    let label = cli
        .files
        .first()
        .map(|p| release_label(p))
        .unwrap_or_else(|| format!("{}", std::process::id()));
    let result = run_single_upload(
        &params,
        &cli.files,
        &label,
        Some(&cancel),
        None,
        false,
        None,
    )
    .await?;

    if let Some(ref p) = session_log {
        write_session_summary(
            p,
            &label,
            result.cancelled,
            result.had_failures,
            result.total_bytes,
            result.nzb_path.as_deref(),
        );
    }

    if result.cancelled {
        std::process::exit(130);
    }
    if result.had_failures {
        std::process::exit(1);
    }
    Ok(())
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

/// Print the orientation screen shown when `pesto` is run with no files.
fn print_header() {
    eprintln!(
        "pesto v{} — fast, lean Usenet poster",
        pesto::DISPLAY_VERSION
    );
    eprintln!("{}", "─".repeat(48));
}

fn print_welcome() {
    let cfg = config::default_config_path();
    let cfg_exists = cfg.as_deref().map(Path::exists).unwrap_or(false);

    println!("pesto — fast, lean Usenet poster\n");
    println!("Getting started:");
    println!("  pesto <PATH>...     post files or directories to Usenet");
    println!("  pesto --config      create your config with a guided wizard");
    println!("  pesto --help        show every option in detail\n");

    match (&cfg, cfg_exists) {
        (Some(path), true) => println!("Config found: {}", path.display()),
        (Some(path), false) => {
            println!("No config yet. Run `pesto --config` to create one at:");
            println!("  {}", path.display());
        }
        (None, _) => println!(
            "Set $HOME or $XDG_CONFIG_HOME so pesto can locate a config file,\n\
             or pass every setting as a flag (see `pesto --help`)."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pesto::config::{FileConfig, Overrides};

    fn resolve_with_tvdb(tvdb_id: &str) -> Config {
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        Config::resolve(
            file,
            Overrides {
                tvdb_id: Some(tvdb_id.to_string()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn tvdb_bare_id_defaults_to_series_category_and_dereferrer() {
        let config = resolve_with_tvdb("81189");
        assert_eq!(config.nzb_category.as_deref(), Some("tv"));
        assert!(
            nfo_metadata_header(&config).contains("https://thetvdb.com/dereferrer/series/81189")
        );
    }

    #[test]
    fn tvdb_movie_ref_sets_movies_category_and_dereferrer() {
        let config = resolve_with_tvdb("movie/123");
        assert_eq!(config.nzb_category.as_deref(), Some("movies"));
        assert!(nfo_metadata_header(&config).contains("https://thetvdb.com/dereferrer/movie/123"));
    }

    #[test]
    fn tvdb_explicit_category_overrides_kind_default() {
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        let config = Config::resolve(
            file,
            Overrides {
                tvdb_id: Some("movie/123".to_string()),
                nzb_category: Some("custom".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(config.nzb_category.as_deref(), Some("custom"));
    }

    #[test]
    fn season_key_standard_sxxexx() {
        assert_eq!(
            season_key("Batwheels.S02E32-E33.1080p.NF.WEB-DL.DDP5.1.H.264.DUAL-BiOMA"),
            Some("Batwheels.S02".into())
        );
        assert_eq!(
            season_key("Show.Name.S01E01.720p.BluRay"),
            Some("Show.Name.S01".into())
        );
        assert_eq!(season_key("Series.s03e05.HDTV"), Some("Series.S03".into()));
    }

    #[test]
    fn season_key_no_season_returns_none() {
        assert_eq!(season_key("Random.Movie.2024.1080p"), None);
        assert_eq!(season_key("file"), None);
    }
}
