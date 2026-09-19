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
use pesto::compress::{compress, existing_archive, random_password, ArchiveFormat};
use pesto::config::{self, Config, FileConfig, ObfuscateMode};
use pesto::logging;
use pesto::nntp::pool::ConnectionBroker;
use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;
use tracing::{error, info};

mod batch;
mod cleanup;
mod cli;
mod hooks;
mod output;
mod season;
mod watch;

use batch::{apply_ext_filter, derive_season_nzb_path, release_label, run_batch};
use cleanup::CleanupMode;
use cli::Cli;
use hooks::{run_all_hooks, run_pre_hook, run_pre_hooks_dir, HookEnv};
use output::{expand_tilde, nzb_archive_path, resolve_nzb_dest};
use watch::{run_watch, WatchBatchOpts};

/// Tracks this process's exact live-heap byte count (see
/// [`pesto::memory::alloc`]), for comparison against `VmSize`/`RLIMIT_AS` in
/// `--memory-report`. Declared here — in the binary, not the `pesto` library
/// — because `#[global_allocator]` is a whole-binary choice; `upapasta`,
/// `penne` and `sugo` link `pesto` as a library and are unaffected by it.
#[global_allocator]
static ALLOC: pesto::memory::alloc::CountingAlloc = pesto::memory::alloc::CountingAlloc::new();

/// Parameters for a single upload job that don't change between entries.
#[derive(Clone)]
struct UploadParams {
    config: Arc<Config>,
    /// The raw `--password` flag value (used to detect "was it auto-generated?").
    archive_password_raw: Option<String>,
    nzb_default: Option<String>,
    json_mode: bool,
    out: Option<PathBuf>,
    /// Write a history record to history.jsonl after each successful upload.
    write_history: bool,
    renderer_opts: pesto::progress::RendererOptions,
    /// Extensions from `--ext`, lowercased with any leading dot stripped.
    /// Empty means no filtering.
    ext_filter: Vec<String>,
    /// Behavior for cleaning up source files/directories after successful upload.
    cleanup_mode: CleanupMode,
}

/// The result of a single upload (one entry in `--each` / `--season`).
struct UploadResult {
    segments: Vec<PostedSegment>,
    groups: Vec<String>,
    cancelled: bool,
    had_failures: bool,
    /// STAT path failed without a 430. Split from `had_failures` so the
    /// season-pack gate can refuse independently of MissingConfirmed.
    inconclusive: Vec<String>,
    total_bytes: u64,
    nzb_path: Option<PathBuf>,
    /// The files actually posted for this entry — post-compression when
    /// `--compress`/`--password` replaced the original input with an
    /// archive. A `--season` batch needs these (not the original episode
    /// paths) to compute a global PAR2 set that matches what's really on
    /// the wire; see `keep_compress_temp` on [`run_single_upload`].
    posted_paths: Vec<PathBuf>,
    /// Set when `keep_compress_temp` was requested and this entry actually
    /// compressed its input: the temp dir holding `posted_paths`, left on
    /// disk (instead of being cleaned up inline) for the caller to remove
    /// once it's done reading those files.
    compress_temp_dir: Option<PathBuf>,
}

/// Per-phase wall-clock timing accumulated during a single upload (26g).
#[derive(Default)]
struct PhaseTimings {
    compress_ms: Option<u128>,
    /// Includes the streaming check/repost queue draining, which now runs
    /// concurrently with posting rather than as a separate serial phase.
    post_ms: Option<u128>,
}

/// Resolve the archive password for one upload.
///
/// Priority: `forced` (a season's shared password, passed down from
/// `run_batch`) beats `explicit` (`Config::compress_password` — an
/// explicit `--password VALUE`, meant to be reused verbatim by every entry
/// in the run) beats a freshly-generated random password when `raw` shows a
/// bare `--password` was given (`Some("")`) beats no password at all.
///
/// The bare-flag case is resolved here, per call, rather than once when the
/// CLI is parsed — resolving it once used to bake a single random password
/// into `Config` for the whole process, so every entry under
/// `--each`/`--watch` silently shared it instead of getting its own
/// (issue #67). `run_batch` passes `forced` for a `--season` batch (every
/// episode needs the same password so the merged season NZB only needs
/// one) and `None` otherwise, so a plain `--each` still gets a fresh
/// password per entry through the `raw` fallback below.
fn resolve_entry_password(
    forced: Option<&str>,
    explicit: Option<&str>,
    raw: Option<&str>,
) -> Option<String> {
    forced
        .or(explicit)
        .map(str::to_string)
        .or_else(|| (raw == Some("")).then(random_password))
}

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

    // Derive NZB stem from: --out > nzb_default > nzb_dir/<stem>.nzb > ./<stem>.nzb
    // Computed from the original entry_paths, before compression, so it never
    // depends on the (possibly obfuscated/randomised) archive name compression
    // produces below.
    //
    // nzb_stem: bare filename without extension, used to name the NZB.
    // nzb_user_dest: optional user-requested destination (--out or nzb_dir).
    //   The canonical copy always goes to ~/.config/pesto/nzb/TIMESTAMP_stem.nzb;
    //   a hardlink (or copy) is placed at nzb_user_dest when set.
    let nzb_stem: Option<String> = params
        .out
        .as_ref()
        .map(|p| {
            p.with_extension("")
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .or_else(|| {
            params.nzb_default.as_deref().map(|s| {
                PathBuf::from(s)
                    .with_extension("")
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .or_else(|| {
            entry_paths
                .first()
                .and_then(|p| {
                    p.file_name().map(|s| {
                        // Release directories use the full folder name as the NZB
                        // stem — calling file_stem() would strip codec tags like
                        // "264" from "H.264" or "0" from "AAC2.0".
                        if p.is_dir() {
                            s.to_string_lossy().into_owned()
                        } else {
                            std::path::Path::new(s)
                                .file_stem()
                                .unwrap_or(s)
                                .to_string_lossy()
                                .into_owned()
                        }
                    })
                })
                .or_else(|| upload_root(&inputs))
                .or_else(|| {
                    inputs.first().map(|f| {
                        let top = f.name.split('/').next().unwrap_or(&f.name);
                        // When the name has a slash, top is a directory component —
                        // use it as-is to avoid stripping codec tags.
                        if f.name.contains('/') {
                            top.to_owned()
                        } else {
                            PathBuf::from(top)
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        }
                    })
                })
        });

    // User-specified destination directory/path for the NZB hardlink.
    // Priority: --out > nzb_dir > directory next to the uploaded file(s).
    let nzb_user_dest: Option<PathBuf> = params.out.clone().or_else(|| {
        nzb_stem.as_deref().and_then(|stem| {
            if let Some(dir) = config.nzb_dir.as_deref() {
                Some(expand_tilde(dir).join(format!("{stem}.nzb")))
            } else {
                // Default: place the NZB next to the uploaded file/directory.
                entry_paths
                    .first()
                    .and_then(|p| {
                        if p.is_dir() {
                            Some(p.as_path())
                        } else {
                            p.parent()
                        }
                    })
                    .map(|d| d.join(format!("{stem}.nzb")))
            }
        })
    });

    // Resume state is keyed to the user-visible stem so it is stable across re-posts.
    let resume_path: Option<PathBuf> = nzb_user_dest
        .as_ref()
        .map(|p| p.with_extension("pesto-state"))
        .or_else(|| {
            nzb_stem
                .as_deref()
                .map(|s| PathBuf::from(s).with_extension("pesto-state"))
        });

    // nzb_out_path is resolved at write time (after post) — placeholder kept for
    // symmetry with the rest of the function.
    let nzb_out_path: Option<String> = nzb_stem.clone();

    // ── Compression ──────────────────────────────────────────────────────────
    let compress_format_str: Option<String> = config.compress_format.clone().or_else(|| {
        if effective_password.is_some() {
            Some("7z".to_string())
        } else {
            None
        }
    });

    let compress_temp_dir: Option<PathBuf>;
    // `light` makes a compressed archive's opaque filename the one public
    // share token: it is reused for the wire, NZB and PAR2 metadata below.
    let mut light_compressed_prefix: Option<String> = None;
    if let Some(fmt_str) = &compress_format_str {
        let format = ArchiveFormat::parse(fmt_str).ok_or_else(|| {
            anyhow::anyhow!("unknown compression format `{fmt_str}`; supported: 7z, zip, rar")
        })?;

        if format == ArchiveFormat::Rar && pesto::compress::find_binary("rar").is_none() {
            eprintln!("note: rar password protection requires the `rar` binary in PATH");
        }

        let client_archive_stem = upload_root(&inputs)
            .or_else(|| {
                inputs.first().map(|f| {
                    PathBuf::from(&f.name)
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
            })
            .unwrap_or_else(|| "archive".to_string());
        let client_archive_stem = pesto::compress::portable_archive_stem(&client_archive_stem);

        // The obfuscated archive name is normally regenerated fresh on every
        // run — but that means a --resume run can never match this file's
        // segments back up, since the resume key is this very name. When a
        // compatible prior state exists (same posting parameters — see
        // `resume::RunFingerprint`) and already recorded one, reuse it
        // instead of generating a new one; otherwise generate fresh and
        // record it (tracked unconditionally, same as segment state — see
        // issue #18's follow-up discussion) so a *future* --resume can reuse
        // it. `poster::post_files_with_progress_and_cancel` still validates
        // the fingerprint itself, so a genuinely incompatible resume run
        // simply gets a fresh stem here and a wiped segment state there.
        let archive_stem = if config.obfuscate != ObfuscateMode::None {
            reuse_or_generate_archive_stem(resume_path.as_deref(), config)
        } else {
            client_archive_stem.clone()
        };
        if config.obfuscate == ObfuscateMode::Light {
            light_compressed_prefix = Some(archive_stem.clone());
        }

        let tmp_base = config
            .compress_temp_dir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        // A pid-keyed directory is gone on the next process, so `--resume`
        // can never see the archive it recorded. When resuming, key the
        // scratch dir by the (stable) archive stem so an interrupted run
        // finds the same files and can skip recompression.
        let tmp_dir = if config.resume {
            tmp_base.join(format!("pesto_compress_{archive_stem}"))
        } else {
            tmp_base.join(format!(
                "pesto_compress_{}_{}",
                std::process::id(),
                entry_label
            ))
        };
        compress_temp_dir = Some(tmp_dir.clone());

        let fs_paths: Vec<PathBuf> = collect_compress_roots(&inputs);
        let compress_input_bytes: u64 = fs_paths.iter().map(|p| dir_or_file_size(p)).sum();

        let t_compress = std::time::Instant::now();
        let _ = progress_tx.send(pesto::progress::ProgressEvent::CompressStarted {
            total_bytes: compress_input_bytes,
        });

        // Sum every file currently in the (per-run, exclusive) tmp_dir
        // rather than watching one fixed name: with --compress-volume-size
        // the compressor writes several `stem.partNN.rar` / `stem.7z.NNN`
        // files instead of a single `stem.<ext>`, and this stays correct in
        // both cases.
        let poll_tx = progress_tx.clone();
        let poll_dir = tmp_dir.clone();
        let poll_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let bytes_written = dir_or_file_size(&poll_dir);
                let _ = poll_tx
                    .send(pesto::progress::ProgressEvent::CompressProgress { bytes_written });
            }
        });

        let compress_inputs = fs_paths.clone();
        let compress_stem = archive_stem.clone();
        let compress_dest = tmp_dir.clone();
        let compress_pass = effective_password.clone();
        let compress_volume_size = config.compress_volume_size.clone();
        let result = if config.resume {
            existing_archive(
                &tmp_dir,
                &archive_stem,
                format,
                compress_volume_size.as_deref(),
            )
        } else {
            None
        };
        let result = if let Some(reused) = result {
            eprintln!(
                "resume: reusing existing archive `{}`",
                reused.path.display()
            );
            reused
        } else {
            tokio::task::spawn_blocking(move || {
                compress(
                    &compress_inputs,
                    &compress_stem,
                    &compress_dest,
                    format,
                    compress_pass.as_deref(),
                    compress_volume_size.as_deref(),
                )
            })
            .await
            .context("compressor task panicked")??
        };

        poll_handle.abort();
        let _ = progress_tx.send(pesto::progress::ProgressEvent::CompressDone);
        let compress_ms = t_compress.elapsed().as_millis();
        info!(elapsed_ms = compress_ms, phase = "compress", "phase done");
        timings.compress_ms = Some(compress_ms);

        inputs = std::iter::once(result.path)
            .chain(result.extra_paths)
            .map(|path| {
                let published_stem = if config.obfuscate == ObfuscateMode::Light {
                    &archive_stem
                } else {
                    &client_archive_stem
                };
                let name =
                    pesto::compress::client_archive_name(&path, &archive_stem, published_stem);
                pesto::walk::InputFile { path, name }
            })
            .collect();

        if let Some(pw) = &effective_password {
            let was_auto = params.archive_password_raw.as_deref() == Some("");
            if was_auto {
                println!("archive password: {pw}");
            }
        }
    } else {
        compress_temp_dir = None;
    }
    // ─────────────────────────────────────────────────────────────────────────

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

/// Collect the unique filesystem paths to pass to the compressor.
fn collect_compress_roots(inputs: &[pesto::walk::InputFile]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for input in inputs {
        let depth = input.name.split('/').count();
        let root = if depth <= 1 {
            input.path.clone()
        } else {
            // Strip `depth - 1` trailing components (everything in `name`
            // after the top-level folder) to land on the top-level folder
            // itself, not its parent. `ancestors().nth(k)` strips `k`
            // trailing components, so `nth(depth)` was one level too high —
            // it landed on the folder's *parent*, which under `--watch`
            // silently pulled in sibling top-level entries (issue #67).
            input
                .path
                .ancestors()
                .nth(depth - 1)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| input.path.clone())
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        inputs.iter().map(|f| f.path.clone()).collect()
    } else {
        roots
    }
}

/// The single root folder shared by every input, or `None` for loose files.
fn upload_root(inputs: &[pesto::walk::InputFile]) -> Option<String> {
    let mut root: Option<&str> = None;
    for input in inputs {
        let (candidate, _) = input.name.split_once('/')?;
        match root {
            Some(existing) if existing != candidate => return None,
            _ => root = Some(candidate),
        }
    }
    root.map(str::to_string)
}

/// Decide the obfuscated archive stem for a `--compress`+`--obfuscate` run:
/// reuse the value a compatible prior `--resume` run recorded, or generate a
/// fresh one and record it for a *future* `--resume` to find.
///
/// Only touches the resume-state file when `--resume` is passed. Doing this
/// unconditionally would conflict with
/// `poster::post_files_with_progress_and_cancel`'s own handling of the same
/// file: when `--resume` is *not* passed, that function deliberately starts
/// from a fresh, empty state (see issue #18 — trusting whatever happens to
/// be on disk without being asked is the exact hazard it guards against),
/// which would silently erase whatever this function wrote moments earlier.
/// The practical result is the same rule as everywhere else in resume
/// handling: a stem only survives into a later run when every run in the
/// chain, including the first, passes `--resume`.
fn reuse_or_generate_archive_stem(resume_path: Option<&Path>, config: &Config) -> String {
    if !config.resume {
        return pesto::article::obfuscated_name();
    }
    let Some(rp) = resume_path else {
        return pesto::article::obfuscated_name();
    };
    let fingerprint = pesto::resume::RunFingerprint::from_config(config);
    let mut state = pesto::resume::ResumeState::load(rp).unwrap_or_default();
    // Normalizes the loaded state first: a fingerprint mismatch clears any
    // stale archive_stem (and segments/files) before we look at it, so an
    // incompatible prior run's name is never reused.
    state.validate_run(&fingerprint);
    if let Some(stem) = state.archive_stem() {
        return stem.to_string();
    }
    let stem = pesto::article::obfuscated_name();
    state.set_archive_stem(stem.clone());
    let _ = state.save(rp);
    stem
}

/// The posting flags that `resume::RunFingerprint` actually checks,
/// formatted for a copy-pasteable `--resume` retry command. A retry using
/// different values for any of these gets its resume state silently (and
/// safely) discarded by `validate_run` — printing them explicitly means a
/// copy-pasted retry command actually resumes instead of quietly re-posting
/// everything from scratch. Closes the gap issue #18 called out: "the
/// printed resume hint only suggests `pesto <file> --resume` and drops the
/// original flags".
fn resume_flags_string(config: &Config) -> String {
    let obfuscate = match config.obfuscate {
        ObfuscateMode::None => "none",
        ObfuscateMode::Full => "full",
        ObfuscateMode::Light => "light",
        ObfuscateMode::FullShared => "full-shared",
        ObfuscateMode::Article => "article",
    };
    let mut flags = format!(
        "--article-size {} --obfuscate={obfuscate} --par2 {}",
        config.article_size, config.par2
    );
    if let Some(fmt) = &config.compress_format {
        flags.push_str(&format!(" --compress={fmt}"));
    }
    if config.file_counter {
        flags.push_str(" --file-counter");
    }
    if let Some(n) = config.par2_slice_size {
        flags.push_str(&format!(" --par2-slice-size {n}"));
    }
    if let Some(n) = config.par2_slice_count {
        flags.push_str(&format!(" --par2-slice-count {n}"));
    }
    if let Some(n) = config.par2_recovery_count {
        flags.push_str(&format!(" --par2-recovery-count {n}"));
    }
    if let Some(v) = &config.compress_volume_size {
        flags.push_str(&format!(" --compress-volume-size {v}"));
    }
    if config.line_length != pesto::yenc::DEFAULT_LINE_LENGTH {
        flags.push_str(&format!(" --line-length {}", config.line_length));
    }
    flags
}

/// Recursively sum bytes for a path that may be a file or a directory.
fn dir_or_file_size(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Err(_) => 0,
        Ok(m) if m.is_file() => m.len(),
        Ok(_) => {
            let mut total = 0u64;
            if let Ok(rd) = std::fs::read_dir(path) {
                for entry in rd.flatten() {
                    total += dir_or_file_size(&entry.path());
                }
            }
            total
        }
    }
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
    use pesto::walk::InputFile;

    fn test_config(
        article_size: usize,
        obfuscate: ObfuscateMode,
        compress_format: Option<&str>,
        par2: u8,
    ) -> Config {
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        Config::resolve(
            file,
            Overrides {
                article_size: Some(article_size),
                obfuscate: Some(obfuscate),
                compress_format: compress_format.map(str::to_string),
                par2: Some(par2),
                ..Default::default()
            },
        )
        .unwrap()
    }

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
    fn resume_flags_string_includes_every_fingerprinted_flag() {
        let config = test_config(384_000, ObfuscateMode::Full, None, 10);
        assert_eq!(
            resume_flags_string(&config),
            "--article-size 384000 --obfuscate=full --par2 10"
        );
    }

    #[test]
    fn resume_flags_string_includes_compress_only_when_set() {
        let none_compressed = test_config(768_000, ObfuscateMode::None, None, 0);
        assert!(!resume_flags_string(&none_compressed).contains("--compress"));

        let compressed = test_config(768_000, ObfuscateMode::FullShared, Some("7z"), 5);
        assert_eq!(
            resume_flags_string(&compressed),
            // `file_counter` defaults to true for `full-shared` — see
            // `Config::resolve`'s obfuscate-mode-dependent default.
            "--article-size 768000 --obfuscate=full-shared --par2 5 --compress=7z --file-counter"
        );
    }

    // ── reuse_or_generate_archive_stem ─────────────────────────────────────

    #[test]
    fn archive_stem_without_resume_is_always_fresh_and_untracked() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = false;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let a = reuse_or_generate_archive_stem(Some(&state_path), &config);
        let b = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_ne!(
            a, b,
            "without --resume, every call must generate a fresh name"
        );
        assert!(
            !state_path.exists(),
            "without --resume, nothing should be written to disk"
        );
    }

    #[test]
    fn archive_stem_without_a_resume_path_is_fresh() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let a = reuse_or_generate_archive_stem(None, &config);
        let b = reuse_or_generate_archive_stem(None, &config);
        assert_ne!(a, b);
    }

    #[test]
    fn archive_stem_is_generated_and_recorded_on_first_resume_run() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let stem = reuse_or_generate_archive_stem(Some(&state_path), &config);

        let state = pesto::resume::ResumeState::load(&state_path).unwrap();
        assert_eq!(state.archive_stem(), Some(stem.as_str()));
    }

    #[test]
    fn archive_stem_is_reused_on_a_compatible_resume_run() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let first = reuse_or_generate_archive_stem(Some(&state_path), &config);
        let second = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_eq!(
            first, second,
            "a compatible resume run must reuse the same stem"
        );
    }

    #[test]
    fn archive_stem_is_regenerated_when_posting_parameters_changed() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let first = reuse_or_generate_archive_stem(Some(&state_path), &config);

        // A later run using a different --article-size: the old stem
        // (recorded under a now-mismatched fingerprint) must not be reused.
        config.article_size = 384_000;
        let second = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_ne!(
            first, second,
            "a fingerprint mismatch must not reuse the old stem"
        );
        let state = pesto::resume::ResumeState::load(&state_path).unwrap();
        assert_eq!(state.archive_stem(), Some(second.as_str()));
    }

    fn inputs(names: &[&str]) -> Vec<InputFile> {
        names
            .iter()
            .map(|n| InputFile {
                path: PathBuf::from(n),
                name: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn upload_root_finds_a_single_shared_directory() {
        assert_eq!(
            upload_root(&inputs(&["Show/ep01.bin", "Show/extras/clip.bin"])),
            Some("Show".to_string())
        );
    }

    #[test]
    fn upload_root_is_none_for_loose_or_mixed_inputs() {
        assert_eq!(upload_root(&inputs(&["a.bin"])), None);
        assert_eq!(upload_root(&inputs(&["A/x.bin", "B/y.bin"])), None);
        assert_eq!(upload_root(&inputs(&["Show/ep01.bin", "loose.bin"])), None);
    }

    #[test]
    fn collect_compress_roots_loose_file_is_the_file_itself() {
        let files = vec![InputFile {
            path: PathBuf::from("/media/downloads/movie.mkv"),
            name: "movie.mkv".to_string(),
        }];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/media/downloads/movie.mkv")]
        );
    }

    #[test]
    fn collect_compress_roots_directory_input_strips_correctly() {
        let files = vec![
            InputFile {
                path: PathBuf::from("/media/Show/ep01.mkv"),
                name: "Show/ep01.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("/media/Show/ep02.mkv"),
                name: "Show/ep02.mkv".to_string(),
            },
        ];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/media/Show")]
        );
    }

    #[test]
    fn collect_compress_roots_nested_subfolder_strips_to_top_level() {
        // Regression test for issue #67: a file nested two levels deep
        // inside the top-level folder (e.g. `Test1/Subs/en.srt`) must still
        // resolve to `Test1`, not to `Test1`'s parent.
        let files = vec![InputFile {
            path: PathBuf::from("/home/user/upload/Test1/Subs/en.srt"),
            name: "Test1/Subs/en.srt".to_string(),
        }];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/home/user/upload/Test1")]
        );
    }

    #[test]
    fn collect_compress_roots_relative_folder_resolves_to_folder_itself() {
        // A directory passed with a bare relative path (e.g. `pesto Test1
        // --compress` run from Test1's parent) must still resolve to
        // `Test1`, not fall back to per-file roots or an empty path.
        let files = vec![
            InputFile {
                path: PathBuf::from("Test1/movie.mkv"),
                name: "Test1/movie.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("Test1/movie.nfo"),
                name: "Test1/movie.nfo".to_string(),
            },
        ];
        assert_eq!(collect_compress_roots(&files), vec![PathBuf::from("Test1")]);
    }

    #[test]
    fn collect_compress_roots_does_not_leak_sibling_top_level_folders() {
        // Regression test for issue #67: compressing `Test1` under
        // `--watch` must never resolve to the watch directory itself, or
        // sibling entries like `Test2` end up bundled into the same
        // archive.
        let files = vec![
            InputFile {
                path: PathBuf::from("/home/user/upload/Test1/movie.mkv"),
                name: "Test1/movie.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("/home/user/upload/Test1/movie.nfo"),
                name: "Test1/movie.nfo".to_string(),
            },
        ];
        let roots = collect_compress_roots(&files);
        assert_eq!(roots, vec![PathBuf::from("/home/user/upload/Test1")]);
        assert!(!roots.contains(&PathBuf::from("/home/user/upload")));
    }

    #[test]
    fn resolve_entry_password_no_flag_is_no_password() {
        assert_eq!(resolve_entry_password(None, None, None), None);
    }

    #[test]
    fn resolve_entry_password_explicit_password_is_reused_verbatim() {
        // `--password mypass`: same literal string every time it's resolved,
        // matching every entry under --each/--season/--watch sharing it.
        for raw in [None, Some(""), Some("mypass")] {
            assert_eq!(
                resolve_entry_password(None, Some("mypass"), raw),
                Some("mypass".to_string())
            );
        }
    }

    #[test]
    fn resolve_entry_password_bare_flag_generates_a_password() {
        // Regression for issue #67: bare `--password` (raw == Some("")) with
        // no forced/explicit password must still produce something to
        // protect the archive with.
        let pw = resolve_entry_password(None, None, Some(""));
        assert!(pw.is_some());
        assert_eq!(pw.as_deref().map(str::len), Some(24));
    }

    #[test]
    fn resolve_entry_password_bare_flag_is_unique_per_call() {
        // Regression for issue #67: under plain --each/--watch (no forced
        // password), every call must mint its own password instead of the
        // whole run sharing one — this is what let `Test1.nzb` and
        // `Test2.nzb` end up with the identical password after the
        // `Cli::overrides()`-time resolution used to bake one value in for
        // the whole process.
        let a = resolve_entry_password(None, None, Some(""));
        let b = resolve_entry_password(None, None, Some(""));
        assert_ne!(a, b);
    }

    #[test]
    fn resolve_entry_password_forced_wins_over_explicit_and_bare() {
        // Regression for issue #67: a --season batch resolves one shared
        // password up front (`run_batch`'s `season_password`) and forces it
        // on every entry — every episode must get that exact value even
        // though each entry, left alone, would otherwise resolve its own
        // (explicit or freshly-random) password.
        assert_eq!(
            resolve_entry_password(Some("season-pw"), Some("explicit-pw"), Some("")),
            Some("season-pw".to_string())
        );
        assert_eq!(
            resolve_entry_password(Some("season-pw"), None, Some("")),
            Some("season-pw".to_string())
        );
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
