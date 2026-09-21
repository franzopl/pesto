//! Event loop and non-key event dispatch.

use std::io;
use std::time::Duration;

use ratatui::{backend::Backend, Terminal};
use tokio::sync::mpsc;

use crate::app::{self, App};
use crate::events::AppEvent;
use crate::{input, prowlarr, tasks, ui};

pub(crate) async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    tx: mpsc::UnboundedSender<AppEvent>,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    upload_log_path: Option<&std::path::Path>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Drain all pending events (non-blocking)
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::Key(key) => {
                    if input::handle_key(app, key, &tx) {
                        return Ok(());
                    }
                }
                event => handle_app_event(app, event, &tx, upload_log_path),
            }
        }

        poll_background(app, &tx);

        // Small sleep to avoid busy-looping the draw thread
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

fn handle_app_event(
    app: &mut App,
    event: AppEvent,
    tx: &mpsc::UnboundedSender<AppEvent>,
    upload_log_path: Option<&std::path::Path>,
) {
    match event {
        AppEvent::Progress(msg) => {
            if let Some(lp) = upload_log_path {
                let m = msg.trim();
                if !m.is_empty() {
                    crate::catalog::append_upload_log(lp, m);
                }
            }
            // Auto-classify ERROR/WARN lines
            let msg_lower = msg.to_lowercase();
            if msg_lower.starts_with("error") || msg_lower.starts_with("failed") {
                app.log_panel.push_error(msg.clone());
                app.status_bar.set(format!("Error: {}", msg));
            } else if msg_lower.starts_with("warn") {
                app.log_panel.push_warn(msg);
            } else {
                app.handle_progress(msg);
            }
        }
        AppEvent::UploadError(msg) => {
            if let Some(lp) = upload_log_path {
                crate::catalog::append_upload_log(lp, &format!("ERROR: {msg}"));
            }
            app.log_panel.push_error(format!("ERROR: {}", msg));
            app.status_bar.set("Upload error — see logs for details");
        }
        AppEvent::ProgressUpdate(update) => {
            app.handle_progress_update(update);
        }
        AppEvent::ItemUploadStarted { path } => {
            app.item_upload_started(&path);
        }
        AppEvent::RegisterFiles { files } => {
            app.register_upload_files(files);
        }
        AppEvent::HooksDone {
            ok,
            release_key,
            release_name,
            hook_name,
            log,
        } => {
            for line in &log {
                app.log_panel.push(line.clone());
            }
            if ok {
                app.record_hook_run(&release_key, &release_name, &hook_name);
                app.status_bar
                    .set(format!("Hook {hook_name} sent for {release_name}"));
            } else {
                app.status_bar
                    .set(log.first().cloned().unwrap_or_else(|| "Hook failed".into()));
            }
        }
        AppEvent::ItemUploadDone {
            path,
            success,
            size_bytes,
            nzb_path,
            duration_s,
            record_catalog,
        } => {
            if let Some(lp) = upload_log_path {
                let status = if success { "OK" } else { "FAILED" };
                crate::catalog::append_upload_log(
                    lp,
                    &format!("{status} {path} ({size_bytes} bytes, {duration_s:.1}s)"),
                );
            }
            app.item_upload_done(
                &path,
                success,
                size_bytes,
                nzb_path,
                duration_s,
                record_catalog,
            );
        }
        AppEvent::CatalogRecord {
            original_name,
            size_bytes,
            nzb_path,
            duration_s,
        } => {
            app.record_catalog_entry(original_name, size_bytes, nzb_path, duration_s, false);
        }
        AppEvent::UploadFinished { success, cancelled } => {
            app.upload_finished(success, cancelled);
            // A manual batch just freed the poster — let a waiting
            // watch item start right away instead of idling until
            // the next poll interval.
            tasks::watch::start_watch_upload(app, tx.clone());
        }
        AppEvent::WatchScanReady { entries } => {
            app.apply_watch_scan(entries);
        }
        AppEvent::WatchUploadDone {
            path,
            success,
            cancelled,
            size_bytes,
            nzb_path,
            duration_s,
        } => {
            app.watch_upload_done(path, success, cancelled, size_bytes, nzb_path, duration_s);
            tasks::watch::start_watch_upload(app, tx.clone());
        }
        AppEvent::ProwlarrStatus(status) => {
            match &status {
                prowlarr::ConnectionStatus::Ok(ver) => {
                    app.status_bar.set(format!("Prowlarr connected — v{}", ver));
                }
                prowlarr::ConnectionStatus::Failed(err) => {
                    app.status_bar.set(format!("Prowlarr error: {}", err));
                }
                _ => {}
            }
            app.prowlarr.status = status;
        }
        AppEvent::ProwlarrSearchDone(result) => {
            if let Some(ref mut s) = app.prowlarr.search {
                s.searching = false;
                match result {
                    Ok(results) => {
                        let n = results.len();
                        s.results = results;
                        s.selected = 0;
                        app.status_bar.set(format!(
                            "Prowlarr: {} result{} for \"{}\"",
                            n,
                            if n == 1 { "" } else { "s" },
                            s.query
                        ));
                    }
                    Err(e) => {
                        s.error = Some(e.clone());
                        app.status_bar.set(format!("Prowlarr search error: {}", e));
                    }
                }
            }
        }
        AppEvent::ProwlarrDownloadDone(result) => {
            if let Some(ref mut s) = app.prowlarr.search {
                s.downloading = false;
            }
            match result {
                Ok(dest) => {
                    let name = dest
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    app.status_bar.set(format!("Downloaded: {}", name));
                    app.prowlarr.search = None;
                    // The new .nzb now backs a release: refresh the disk
                    // index so the Browser shows the [✓] badge for it.
                    app.refresh_nzb_disk_index();
                    if app.state == app::AppState::NzbVault {
                        app.load_vault();
                    }
                }
                Err(e) => {
                    app.status_bar.set(format!("Download failed: {}", e));
                }
            }
        }
        AppEvent::ProwlarrBatchProgress {
            done,
            total,
            current,
            downloaded,
            no_match,
            failed,
            log,
        } => {
            app.log_panel.push(log);
            app.prowlarr.batch = Some(app::ProwlarrBatchState {
                done,
                total,
                downloaded,
                no_match,
                failed,
                current,
            });
            app.status_bar.set(format!(
                "Prowlarr queue search {}/{} — {} fetched, {} no match, {} failed",
                done, total, downloaded, no_match, failed
            ));
        }
        AppEvent::ProwlarrBatchDone {
            downloaded,
            no_match,
            failed,
        } => {
            app.prowlarr.batch = None;
            app.status_bar.set(format!(
                "Queue search done — {} fetched, {} no match, {} failed",
                downloaded, no_match, failed
            ));
            app.log_panel.push(format!(
                "=== Prowlarr queue search done: {} fetched · {} no match · {} failed ===",
                downloaded, no_match, failed
            ));
            // Newly downloaded NZBs back queued releases: refresh badges.
            if downloaded > 0 {
                app.refresh_nzb_disk_index();
                if app.state == app::AppState::NzbVault {
                    app.load_vault();
                }
            }
        }
        AppEvent::Tick => {
            app.tick_count = app.tick_count.wrapping_add(1);
        }
        AppEvent::DirScanReady {
            generation,
            results,
        } => {
            app.file_tree.apply_scan(generation, results);
        }
        AppEvent::QueueMetaReady {
            key,
            file_count,
            size_bytes,
        } => {
            app.apply_queue_meta(&key, file_count, size_bytes);
        }
        _ => {}
    }
}

fn poll_background(app: &mut App, tx: &mpsc::UnboundedSender<AppEvent>) {
    // Off-thread folder sizing: queueing a folder needs a recursive walk to
    // count files / sum bytes. Run each pending walk on a blocking worker
    // and fold the result back via QueueMetaReady.
    for key in app.take_pending_meta() {
        let tx_meta = tx.clone();
        tokio::task::spawn_blocking(move || {
            let (file_count, size_bytes) = app::dir_stats(std::path::Path::new(&key));
            let _ = tx_meta.send(AppEvent::QueueMetaReady {
                key,
                file_count,
                size_bytes,
            });
        });
    }

    // Off-thread directory scan: the Browser's backed/size summary needs
    // recursive filesystem walks that would otherwise freeze the UI loop
    // while navigating large folders. Hand any pending scan to a blocking
    // worker and fold the result back in via DirScanReady.
    if let Some(job) = app.file_tree.take_scan_job() {
        let tx_scan = tx.clone();
        tokio::task::spawn_blocking(move || {
            let (generation, results) = job.run();
            let _ = tx_scan.send(AppEvent::DirScanReady {
                generation,
                results,
            });
        });
    }

    // Watch mode: periodically rescan the configured directory off-thread.
    // `scanning` guards against a scan overlapping the next poll tick if
    // a directory listing is unusually slow.
    if app.watch.enabled && !app.watch.scanning {
        let due = app
            .watch
            .last_scan
            .map(|t| t.elapsed() >= Duration::from_secs(app.watch.interval_secs))
            .unwrap_or(true);
        if due {
            if let Some(dir) = app.watch.dir.clone() {
                app.watch.scanning = true;
                let ext_filter = app.watch.ext_filter.clone();
                let tx_watch = tx.clone();
                tokio::task::spawn_blocking(move || {
                    let entries = tasks::watch::scan_watch_dir(&dir, &ext_filter);
                    let _ = tx_watch.send(AppEvent::WatchScanReady { entries });
                });
            }
        }
    }

    // Watch mode: start the next stabilized item once the poster is free.
    // (Also triggered right after a manual/watch upload finishes, above —
    // this covers the case where items became ready while nothing was
    // running at all.)
    if app.watch.enabled && !app.upload_in_progress {
        tasks::watch::start_watch_upload(app, tx.clone());
    }
}
