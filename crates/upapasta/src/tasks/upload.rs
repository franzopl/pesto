//! Real upload execution, dry-run config and season pack tasks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use pesto::config::{Config, ObfuscateMode};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::app::{self, App};
use crate::events::{AppEvent, ProgressUpdate};

use super::progress::{extract_progress_update, format_progress_event, write_session_summary};
use super::season::season_pack_skip_message;

/// Called when the user presses 'u' on the Dashboard.
/// Delegates the full upload pipeline to `pesto::upload::run_upload`, which
/// handles compression, posting, NZB writing, history, indexer, and hooks.
pub(crate) fn handle_upload_trigger(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    app.trigger_upload();

    let entry_paths: Vec<PathBuf> = app.upload_queue.items.iter().map(PathBuf::from).collect();
    if entry_paths.is_empty() {
        return;
    }

    let config = if let Some(mut real_cfg) = app.effective_config_with_overrides() {
        real_cfg.dry_run = false;
        real_cfg
    } else {
        build_dry_run_config()
    };

    // How directories in the queue become NZB(s): one release NZB (default),
    // one NZB per file, or per-file + a combined season NZB.
    let folder_mode = app.effective_folder_mode();

    let cancel_token = app.current_cancel_token.clone().unwrap_or_default();
    let pause_flag = app
        .current_pause_flag
        .clone()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

    // Direct uploads into nzb_dir/uploaded/ so the vault can distinguish them
    // from downloaded and manually-placed NZBs.
    let nzb_out_dir: Option<PathBuf> = app
        .pesto_config
        .as_ref()
        .and_then(|c| c.nzb_dir.as_deref())
        .map(|d| app::expand_tilde(d).join("uploaded"));
    if let Some(ref d) = nzb_out_dir {
        let _ = std::fs::create_dir_all(d);
    }

    // Each queue item is uploaded in sequence. A directory becomes one release
    // NZB (Single), one NZB per file (PerFile), or per-file NZBs plus a combined
    // season NZB (Season). Files always upload as a single NZB.
    tokio::spawn(async move {
        let total = entry_paths.len();
        let mut any_cancelled = false;
        let mut all_ok = true;
        'outer: for (i, path) in entry_paths.iter().enumerate() {
            let key = path.to_string_lossy().into_owned();
            let label = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("file-{}", i + 1));
            if total > 1 {
                let _ = tx.send(AppEvent::Progress(format!(
                    "=== Upload {}/{}: {} ===",
                    i + 1,
                    total,
                    label
                )));
            }
            let _ = tx.send(AppEvent::ItemUploadStarted { path: key.clone() });
            let item_start = Instant::now();

            let expand = path.is_dir() && folder_mode != app::FolderMode::Single;

            if !expand {
                // One NZB for this entry (a file, or a folder as a single release).
                let result = run_real_upload(
                    config.clone(),
                    vec![path.clone()],
                    label,
                    nzb_out_dir.clone(),
                    tx.clone(),
                    cancel_token.clone(),
                    pause_flag.clone(),
                )
                .await;
                let duration_s = item_start.elapsed().as_secs_f64();
                match result {
                    Err(ref e) => {
                        let _ = tx.send(AppEvent::UploadError(e.to_string()));
                        let _ = tx.send(AppEvent::ItemUploadDone {
                            path: key,
                            success: false,
                            size_bytes: 0,
                            nzb_path: None,
                            duration_s,
                            record_catalog: false,
                        });
                        all_ok = false;
                    }
                    Ok(ref o) if o.cancelled => {
                        any_cancelled = true;
                        break;
                    }
                    Ok(o) => {
                        let success = !o.had_failures;
                        if !success {
                            all_ok = false;
                        }
                        let _ = tx.send(AppEvent::ItemUploadDone {
                            path: key,
                            success,
                            size_bytes: o.total_bytes,
                            nzb_path: o.nzb_path,
                            duration_s,
                            record_catalog: true,
                        });
                    }
                }
                continue;
            }

            // PerFile / Season: expand the folder into its files and upload each
            // as its own NZB, recording each in the catalog as it lands.
            let files = match pesto::walk::expand_inputs(std::slice::from_ref(path)) {
                Ok(f) => f,
                Err(e) => {
                    let _ = tx.send(AppEvent::UploadError(format!("expand {label}: {e}")));
                    let _ = tx.send(AppEvent::ItemUploadDone {
                        path: key,
                        success: false,
                        size_bytes: 0,
                        nzb_path: None,
                        duration_s: item_start.elapsed().as_secs_f64(),
                        record_catalog: false,
                    });
                    all_ok = false;
                    continue;
                }
            };

            // For Season mode, force resume=true so a .pesto-state file is
            // written per episode. This lets a retry pass skip already-posted
            // segments and re-send only the parts that the server rejected.
            let episode_config = if folder_mode == app::FolderMode::Season {
                let mut c = config.clone();
                c.resume = true;
                c
            } else {
                config.clone()
            };

            let mut all_segments = Vec::new();
            let mut total_size = 0u64;
            let mut folder_ok = true;
            let mut failed_indices: Vec<usize> = Vec::new();
            for (ep_idx, inf) in files.iter().enumerate() {
                let ep_name = inf
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| inf.name.clone());
                let ep_start = Instant::now();
                let result = run_real_upload(
                    episode_config.clone(),
                    vec![inf.path.clone()],
                    ep_name.clone(),
                    nzb_out_dir.clone(),
                    tx.clone(),
                    cancel_token.clone(),
                    pause_flag.clone(),
                )
                .await;
                let ep_dur = ep_start.elapsed().as_secs_f64();
                match result {
                    Err(ref e) => {
                        let _ = tx.send(AppEvent::UploadError(e.to_string()));
                        folder_ok = false;
                        failed_indices.push(ep_idx);
                    }
                    Ok(ref o) if o.cancelled => {
                        any_cancelled = true;
                        break 'outer;
                    }
                    Ok(o) => {
                        if o.had_failures {
                            folder_ok = false;
                            // Don't accumulate partial segments here; the retry
                            // pass will contribute the complete set once the
                            // missing parts are re-posted via resume state.
                            failed_indices.push(ep_idx);
                        } else {
                            total_size += o.total_bytes;
                            let _ = tx.send(AppEvent::CatalogRecord {
                                original_name: ep_name,
                                size_bytes: o.total_bytes,
                                nzb_path: o.nzb_path,
                                duration_s: ep_dur,
                            });
                            all_segments.extend(o.segments);
                        }
                    }
                }
            }

            // Season retry pass: re-upload only the episodes that had segment
            // failures. Resume state written during the first pass lets the
            // poster skip segments that already landed on the server, so only
            // the truly missing parts are re-sent. If every failed episode
            // recovers, folder_ok is restored and the season NZB is generated
            // as normal; if any episode still fails, folder_ok stays false and
            // the incomplete pack is not forwarded to the indexer.
            if folder_mode == app::FolderMode::Season
                && !failed_indices.is_empty()
                && !any_cancelled
            {
                let _ = tx.send(AppEvent::Progress(format!(
                    "retrying {} failed episode(s)...",
                    failed_indices.len()
                )));
                let mut all_retried = true;
                for ep_idx in &failed_indices {
                    let inf = &files[*ep_idx];
                    let ep_name = inf
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| inf.name.clone());
                    let ep_start = Instant::now();
                    let result = run_real_upload(
                        episode_config.clone(),
                        vec![inf.path.clone()],
                        ep_name.clone(),
                        nzb_out_dir.clone(),
                        tx.clone(),
                        cancel_token.clone(),
                        pause_flag.clone(),
                    )
                    .await;
                    let ep_dur = ep_start.elapsed().as_secs_f64();
                    match result {
                        Ok(ref o) if o.cancelled => {
                            any_cancelled = true;
                            break 'outer;
                        }
                        Ok(o) if !o.had_failures => {
                            total_size += o.total_bytes;
                            let _ = tx.send(AppEvent::CatalogRecord {
                                original_name: ep_name,
                                size_bytes: o.total_bytes,
                                nzb_path: o.nzb_path,
                                duration_s: ep_dur,
                            });
                            all_segments.extend(o.segments);
                        }
                        Err(ref e) => {
                            let _ = tx.send(AppEvent::UploadError(format!("retry failed: {e}")));
                            all_retried = false;
                        }
                        Ok(_) => {
                            all_retried = false;
                        }
                    }
                }
                if all_retried {
                    folder_ok = true;
                }
            }

            // Season: consolidate every posted segment into one combined NZB.
            // The pack is a distinct artefact: --allow-incomplete-nzb never
            // unlocks it. Per-episode NZBs already followed nzb_write_decision.
            let mut season_nzb = None;
            if folder_mode == app::FolderMode::Season {
                match season_pack_skip_message(any_cancelled, folder_ok, all_segments.is_empty()) {
                    None => {
                        if let Some(ref dir) = nzb_out_dir {
                            let out = dir.join(format!("{label}.nzb"));
                            let meta = pesto::nzb::NzbMeta {
                                name: Some(label.clone()),
                                password: config
                                    .nzb_password
                                    .clone()
                                    .or_else(|| config.compress_password.clone()),
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
                                tags: config.nzb_tags.clone(),
                            };
                            let xml = pesto::nzb::generate(
                                &config.groups,
                                &all_segments,
                                &meta,
                                config.obfuscate,
                            );
                            match std::fs::write(&out, xml) {
                                Ok(()) => {
                                    let _ = tx.send(AppEvent::Progress(format!(
                                        "wrote season nzb: {}",
                                        out.display()
                                    )));
                                    let _ = tx.send(AppEvent::CatalogRecord {
                                        original_name: label.clone(),
                                        size_bytes: total_size,
                                        nzb_path: Some(out.clone()),
                                        duration_s: item_start.elapsed().as_secs_f64(),
                                    });

                                    // Run post-upload hooks on the combined season pack so
                                    // it reaches the indexer just like each episode does.
                                    // Per-episode hooks run inside `run_upload`; this NZB is
                                    // written here, outside that pipeline, so without this
                                    // the season pack is posted but never sent on. Skip when
                                    // an episode failed — an incomplete pack must not be
                                    // forwarded to the indexer (matches run_upload, which
                                    // only runs hooks when there were no failures).
                                    if folder_ok {
                                        run_season_hooks(
                                            &config, path, &label, &out, total_size, &tx,
                                        )
                                        .await;
                                    }

                                    season_nzb = Some(out);
                                }
                                Err(e) => {
                                    let _ = tx.send(AppEvent::UploadError(format!(
                                        "season nzb write: {e}"
                                    )));
                                    folder_ok = false;
                                }
                            }
                        }
                    }
                    Some(msg) => {
                        if any_cancelled {
                            let _ = tx.send(AppEvent::Progress(msg.to_string()));
                        } else {
                            let _ = tx.send(AppEvent::UploadError(msg.to_string()));
                        }
                    }
                }
            }

            if !folder_ok {
                all_ok = false;
            }
            let _ = tx.send(AppEvent::ItemUploadDone {
                path: key,
                success: folder_ok,
                size_bytes: total_size,
                nzb_path: season_nzb,
                duration_s: item_start.elapsed().as_secs_f64(),
                record_catalog: false,
            });
        }
        let _ = tx.send(AppEvent::UploadFinished {
            success: all_ok && !any_cancelled,
            cancelled: any_cancelled,
        });
    });
}

/// Constructs a minimal Config that exercises the full hot path in dry-run mode.
pub(crate) fn build_dry_run_config() -> Config {
    Config {
        host: "dry-run.local".into(),
        port: 563,
        ssl: true,
        connections: 2,
        username: None,
        password: None,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        proxy: None,
        proxy_check_ip: false,
        extra_servers: vec![],
        from: "upapasta@local".into(),
        groups: vec!["alt.binaries.test".into()],
        article_size: 768_000,
        line_length: 128,
        retries: 2,
        obfuscate: ObfuscateMode::None,
        date: None,
        no_archive: true,
        file_counter: false,
        message_id_domain: None,
        dry_run: true, // ← never touches the network
        par2: 5,
        par2_memory_limit: None,
        memory_limit: None,
        par2_temp_dir: None,
        compress_temp_dir: None,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_only: false,
        par2_before_upload: false,
        threads: 0,
        simd: parmesan::SimdPath::Auto,
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        compress_volume_size: None,
        nzb_title: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: Vec::new(),
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
        tvdb_kind: None,
        mal_id: None,
        nzb_dir: None,
        indexer_url: None,
        indexer_api_key: None,
        history: false,
        history_dir: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: true,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 0,
        check_retries: 1,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        check_recover_percent: 15,
        check_recover_max: 0,
        pipeline_depth: 0,
        keepalive_interval: 60,
    }
}

/// Runs the full upload pipeline via `pesto::upload::run_upload` and streams
/// `ProgressEvent`s to the TUI in real time.
///
/// NZB writing, history recording, indexer upload, notifications, and
/// post-upload hooks are all handled inside `run_upload`; the TUI receives
/// them as `Status` events on the same channel.
/// Run the post-upload hooks on the combined season-pack NZB.
///
/// Mirrors the NFO + hook stage of [`pesto::upload::run_upload`] (which only
/// fires for the per-episode runs): generate a season `.nfo` next to the pack
/// `.nzb` when NFOs are enabled, build the same [`pesto::hooks::HookContext`],
/// then run every configured hook so the pack is forwarded to the indexer like
/// the episodes.
async fn run_season_hooks(
    config: &Config,
    season_dir: &Path,
    label: &str,
    nzb_path: &Path,
    total_bytes: u64,
    tx: &mpsc::UnboundedSender<AppEvent>,
) {
    if config.no_hooks {
        return;
    }

    // Best-effort season NFO from the folder's media (via mediainfo), written
    // next to the pack NZB so hooks that forward an NFO get one.
    let nfo_path = if config.nfo {
        let dest = nzb_path.with_extension("nfo");
        match pesto::nfo::generate_season(std::slice::from_ref(&season_dir.to_path_buf())) {
            Some(content) => match pesto::nfo::write(&dest, &content) {
                Ok(()) => {
                    let _ = tx.send(AppEvent::Progress(format!(
                        "wrote season nfo: {}",
                        dest.display()
                    )));
                    Some(dest)
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::Progress(format!("season nfo write failed: {e}")));
                    None
                }
            },
            None => None,
        }
    } else {
        None
    };

    // No per-segment server record survives the season-pack merge on disk,
    // so — like the pre-hook case — report every configured server rather
    // than just the primary.
    let season_servers_str = config
        .all_servers()
        .map(|s| s.host)
        .collect::<Vec<_>>()
        .join(":");
    let ctx = pesto::hooks::HookContext {
        name: label.to_string(),
        total_bytes,
        input_paths: String::new(),
        server: season_servers_str
            .split(':')
            .next()
            .unwrap_or(&config.host)
            .to_string(),
        servers: season_servers_str,
        group: config.groups.first().cloned().unwrap_or_default(),
        groups: config.groups.join(":"),
        password: config
            .nzb_password
            .as_deref()
            .or(config.compress_password.as_deref())
            .unwrap_or("")
            .to_string(),
        category: config.nzb_category.clone().unwrap_or_default(),
        nzb_title: config.nzb_title.clone().unwrap_or_default(),
        obfuscate: match config.obfuscate {
            ObfuscateMode::None => "none",
            ObfuscateMode::Full => "full",
            ObfuscateMode::Light => "light",
            ObfuscateMode::FullShared => "full-shared",

            ObfuscateMode::Article => "article",
        }
        .to_string(),
        par2: config.par2,
        tags: config.nzb_tags.join(" "),
        nzb_path: nzb_path.to_string_lossy().into_owned(),
        nfo_path: nfo_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        // No per-segment record survives the season-pack merge on disk
        // either — same reasoning as `server`/`servers` above — so there is
        // no wire identity left to report.
        wire_subject: String::new(),
        incomplete: false,
    };

    let hook_cfg = config.clone();
    let log_lines = tokio::task::spawn_blocking(move || pesto::hooks::run_hooks(&hook_cfg, &ctx))
        .await
        .unwrap_or_else(|e| vec![format!("hook task panicked: {e}")]);
    for line in log_lines {
        let _ = tx.send(AppEvent::Progress(format!("[hook] {line}")));
    }
}

pub(crate) async fn run_real_upload(
    config: Config,
    entry_paths: Vec<PathBuf>,
    label: String,
    nzb_out_dir: Option<PathBuf>,
    tx: mpsc::UnboundedSender<AppEvent>,
    cancel_token: CancellationToken,
    pause_flag: Arc<AtomicBool>,
) -> anyhow::Result<pesto::upload::UploadOutcome> {
    // Route pesto's internal DEBUG traces to a per-upload session log file.
    // This mirrors what the pesto CLI does via --session-log.
    let session_log_path =
        pesto::history::session_log_path(config.history_dir.as_deref(), &label, 50);
    if let Some(ref p) = session_log_path {
        let _ = pesto::logging::set_session_log(p);
    }

    // Bridge CancellationToken + signals → AtomicBool (pesto's cancel mechanism).
    let cancel_flag = Arc::new(AtomicBool::new(false));
    {
        let flag = cancel_flag.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel_token.cancelled() => {},
                _ = tokio::signal::ctrl_c() => {},
            }
            flag.store(true, Ordering::Relaxed);
        });
    }

    let (prog_tx, mut prog_rx) =
        tokio::sync::mpsc::unbounded_channel::<pesto::progress::ProgressEvent>();

    // Spawn the full pipeline as a concurrent task so we can drain events live.
    let cfg = config.clone();
    let paths = entry_paths.clone();
    let lbl = label.clone();
    // Resolve the full NZB output path (dir + stem.nzb) when a subdir is given.
    // A directory keeps its full name (release names contain dots that are not
    // extensions); a plain file has a single extension stripped.
    let nzb_override = nzb_out_dir.map(|dir| {
        let stem = entry_paths
            .first()
            .map(|p| app::queue_entry_info(&p.to_string_lossy()).nzb_name)
            .unwrap_or_else(|| label.clone());
        dir.join(format!("{stem}.nzb"))
    });
    let upload_handle = tokio::spawn(async move {
        pesto::upload::run_upload(
            &cfg,
            &paths,
            &lbl,
            Some(prog_tx),
            Some(cancel_flag),
            nzb_override,
            true,
            Some(pause_flag),
        )
        .await
    });

    let mut last_update = ProgressUpdate {
        done_segments: 0,
        total_segments: 0,
        done_bytes: 0,
        total_bytes: 0,
        current_speed_mbps: 0.0,
        message: None,
        file_update: None,
        phase: None,
        par2_slices: None,
        check_progress: None,
        queue_extended: None,
        par2_hint_bytes: 0,
        par2_segment_hint: 0,
        par2_complete: false,
    };

    // select! races the pipeline task against the progress channel.
    // After a terminal event (Finished/Interrupted/Failed) we disable the
    // channel arm; the task arm fires next and we collect the outcome.
    // Any events buffered after Finished (NZB written, hook lines) are drained
    // via try_recv when the task arm fires.
    tokio::pin!(upload_handle);
    let mut events_done = false;

    let outcome = loop {
        tokio::select! {
            result = &mut upload_handle => {
                while let Ok(event) = prog_rx.try_recv() {
                    let msg = format_progress_event(&event);
                    if !msg.is_empty() {
                        let _ = tx.send(AppEvent::Progress(msg));
                    }
                    if let Some(update) = extract_progress_update(&event, &last_update) {
                        last_update = update.clone();
                        let _ = tx.send(AppEvent::ProgressUpdate(update));
                    }
                }
                break match result {
                    Ok(Ok(o)) => o,
                    Ok(Err(e)) => return Err(e),
                    Err(e) => return Err(anyhow::anyhow!("upload task panicked: {e}")),
                };
            }
            event = prog_rx.recv(), if !events_done => {
                let Some(event) = event else {
                    // Channel closed — run_upload() has returned.
                    events_done = true;
                    continue;
                };
                let msg = format_progress_event(&event);
                if !msg.is_empty() {
                    let _ = tx.send(AppEvent::Progress(msg));
                }
                // Seed per-file rows from the run's work plan so per-episode
                // bars (folder modes post each inner file under its real_name)
                // have totals and match later SegmentDone events.
                if let pesto::progress::ProgressEvent::Started { files, .. } = &event {
                    let regs = files
                        .iter()
                        .map(|f| (f.name.clone(), f.segments, f.bytes))
                        .collect();
                    let _ = tx.send(AppEvent::RegisterFiles { files: regs });
                }
                // When the poster finishes, clamp the bar to 100% so the UI
                // shows completion while NZB writing and hooks are still running.
                if matches!(event, pesto::progress::ProgressEvent::Finished) {
                    let done = ProgressUpdate {
                        done_segments: last_update.total_segments,
                        total_segments: last_update.total_segments,
                        done_bytes: last_update.total_bytes,
                        total_bytes: last_update.total_bytes,
                        current_speed_mbps: 0.0,
                        message: None,
                        file_update: None,
                        phase: last_update.phase.clone(),
                        par2_slices: None,
            check_progress: None,
                        queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
                    };
                    last_update = done.clone();
                    let _ = tx.send(AppEvent::ProgressUpdate(done));
                } else if let Some(update) = extract_progress_update(&event, &last_update) {
                    last_update = update.clone();
                    let _ = tx.send(AppEvent::ProgressUpdate(update));
                }
                // Do NOT set events_done on Finished — run_upload() continues
                // after posting (NZB, hooks) and sends more Status events.
                // The channel closes naturally when run_upload() returns.
                if matches!(
                    event,
                    pesto::progress::ProgressEvent::Interrupted
                        | pesto::progress::ProgressEvent::Failed { .. }
                ) {
                    events_done = true;
                }
            }
        }
    };

    let _ = tx.send(AppEvent::Progress(format!(
        "PostOutcome: {} segments, failures: {}",
        outcome.segments.len(),
        outcome.had_failures,
    )));

    // Write a one-line summary to the session log before closing it. This
    // ensures the file is never empty: even a clean run leaves a record with
    // segment count, byte total, and final status.
    if let Some(ref p) = session_log_path {
        write_session_summary(p, &label, &outcome);
    }

    // Stop writing pesto's traces to the per-upload file.
    pesto::logging::clear_session_log();

    Ok(outcome)
}
