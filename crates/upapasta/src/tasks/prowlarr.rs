//! Prowlarr connection, search and download tasks.

use tokio::sync::mpsc;

use crate::app::{self, App};
use crate::events::AppEvent;
use crate::prowlarr;

/// Called when the user presses 'C' on the Config screen.
/// Spawns an async task that tests the Prowlarr connection and sends the result back.
pub(crate) fn trigger_prowlarr_check(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    use prowlarr::ConnectionStatus;

    let cfg = app.prowlarr.resolve(app.pesto_config.as_ref());
    let Some(cfg) = cfg else {
        app.status_bar
            .set("Prowlarr not configured — set URL and API key first");
        app.prowlarr.status = ConnectionStatus::Failed("not configured".into());
        return;
    };

    app.prowlarr.status = ConnectionStatus::Checking;
    app.status_bar.set("Checking Prowlarr connection…");

    tokio::spawn(async move {
        let status = match prowlarr::build_client() {
            Ok(client) => match prowlarr::check_connection(&cfg, &client).await {
                Ok(ver) => ConnectionStatus::Ok(ver),
                Err(e) => ConnectionStatus::Failed(e.to_string()),
            },
            Err(e) => ConnectionStatus::Failed(e.to_string()),
        };
        let _ = tx.send(AppEvent::ProwlarrStatus(status));
    });
}

/// Called when the user presses 'P' in Browser or NZB Vault.
///
/// Derives the release name from the selected filename, opens the search
/// overlay, and spawns an async search task.
pub(crate) fn trigger_prowlarr_search(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    use app::ProwlarrSearchState;

    let cfg = app.prowlarr.resolve(app.pesto_config.as_ref());
    let Some(cfg) = cfg else {
        app.status_bar
            .set("Prowlarr not configured — set URL and API key in Config (F5)");
        return;
    };

    // Derive the release name from the selected path (Browser or Vault).
    // A directory selection keeps its name verbatim; only a file has an
    // extension to strip.
    let selection: Option<(String, bool)> = match app.state {
        app::AppState::Browser => app.file_tree.get_selected().and_then(|p| {
            p.file_name()
                .map(|n| (n.to_string_lossy().into_owned(), p.is_dir()))
        }),
        app::AppState::NzbVault => app.vault.selected_entry().map(|e| (e.name.clone(), false)),
        _ => None,
    };

    let Some((filename, is_dir)) = selection else {
        app.status_bar.set("Nothing selected to search");
        return;
    };

    // A release folder has no extension — stripping after the last dot would
    // drop a group tag and break the match. Only a file's extension is stripped.
    let release_name = if is_dir {
        filename.clone()
    } else {
        prowlarr::release_name_from_filename(&filename).to_string()
    };

    app.status_bar
        .set(format!("Searching Prowlarr for \"{}\"…", release_name));
    app.prowlarr.search = Some(ProwlarrSearchState::new(release_name.clone()));

    tokio::spawn(async move {
        let result = match prowlarr::build_client() {
            Ok(client) => {
                // Restrict to Usenet indexers (best-effort; an empty list falls
                // back to protocol filtering inside search_by_release).
                let ids = prowlarr::usenet_indexer_ids(&cfg, &client)
                    .await
                    .unwrap_or_default();
                prowlarr::search_by_release(&cfg, &client, &release_name, &ids)
                    .await
                    .map_err(|e| format!("{:#}", e))
            }
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(AppEvent::ProwlarrSearchDone(result));
    });
}

/// Called when the user presses 'p' on the Queue screen.
///
/// Searches Prowlarr for every queued release in one background pass and
/// auto-downloads any result whose name matches the release exactly (same
/// [`release_key`]). Items without an exact match are only counted/logged —
/// never auto-downloaded. Progress is streamed back via `ProwlarrBatchProgress`
/// and a final `ProwlarrBatchDone`.
pub(crate) fn trigger_prowlarr_queue_search(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    use crate::ui::components::file_tree::release_key;

    if app.prowlarr.batch.is_some() {
        app.status_bar.set("Queue search already running");
        return;
    }

    let cfg = app.prowlarr.resolve(app.pesto_config.as_ref());
    let Some(cfg) = cfg else {
        app.status_bar
            .set("Prowlarr not configured — set URL and API key in Config (F5)");
        return;
    };

    let nzb_dir = app
        .pesto_config
        .as_ref()
        .and_then(|c| c.nzb_dir.as_deref())
        .map(app::expand_tilde);
    let Some(nzb_dir) = nzb_dir else {
        app.status_bar
            .set("nzb_dir not configured — set it in pesto.toml");
        return;
    };

    let items: Vec<String> = app.upload_queue.items.clone();
    if items.is_empty() {
        app.status_bar.set("Queue is empty — nothing to search");
        return;
    }

    let total = items.len();
    app.prowlarr.batch = Some(app::ProwlarrBatchState {
        total,
        ..Default::default()
    });
    app.status_bar
        .set(format!("Searching Prowlarr for {total} queued release(s)…"));
    app.log_panel
        .push(format!("=== Prowlarr queue search: {total} item(s) ==="));

    tokio::spawn(async move {
        let client = match prowlarr::build_client() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(AppEvent::ProwlarrBatchProgress {
                    done: 0,
                    total,
                    current: String::new(),
                    downloaded: 0,
                    no_match: 0,
                    failed: total,
                    log: format!("✗ HTTP client error: {e}"),
                });
                let _ = tx.send(AppEvent::ProwlarrBatchDone {
                    downloaded: 0,
                    no_match: 0,
                    failed: total,
                });
                return;
            }
        };

        // Discover Usenet indexers once and reuse for every item.
        let ids = prowlarr::usenet_indexer_ids(&cfg, &client)
            .await
            .unwrap_or_default();

        let (mut downloaded, mut no_match, mut failed) = (0usize, 0usize, 0usize);

        for (i, path) in items.iter().enumerate() {
            let p = std::path::Path::new(path);
            let filename = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone());
            // A release folder has no extension — stripping after the last dot
            // would drop a group tag (e.g. `.x264-GROUP`) and break the match.
            // Only plain files carry a container/.nzb extension to strip.
            let release_name = if p.is_dir() {
                filename.clone()
            } else {
                prowlarr::release_name_from_filename(&filename).to_string()
            };
            let want_key = release_key(&filename);

            let log = match prowlarr::search_by_release(&cfg, &client, &release_name, &ids).await {
                Ok(results) => {
                    // Exact match: a result whose title normalizes to the same
                    // release key as the queued file.
                    let exact = results
                        .iter()
                        .find(|r| release_key(&r.title) == want_key && !want_key.is_empty());
                    match exact {
                        Some(result) => {
                            let dest = prowlarr::dest_path_in(&nzb_dir, result);
                            match prowlarr::download_nzb(&cfg, &client, result, &dest).await {
                                Ok(()) => {
                                    downloaded += 1;
                                    format!(
                                        "✓ {release_name} → {}",
                                        prowlarr::nzb_filename_for(result)
                                    )
                                }
                                Err(e) => {
                                    failed += 1;
                                    format!("✗ {release_name}: download failed: {e}")
                                }
                            }
                        }
                        None => {
                            no_match += 1;
                            format!(
                                "– {release_name}: no exact match ({} result(s))",
                                results.len()
                            )
                        }
                    }
                }
                Err(e) => {
                    failed += 1;
                    format!("✗ {release_name}: search failed: {e}")
                }
            };

            let _ = tx.send(AppEvent::ProwlarrBatchProgress {
                done: i + 1,
                total,
                current: release_name,
                downloaded,
                no_match,
                failed,
                log,
            });
        }

        let _ = tx.send(AppEvent::ProwlarrBatchDone {
            downloaded,
            no_match,
            failed,
        });
    });
}

/// Called when the user presses 'd' on a search result to download its NZB.
pub(crate) fn trigger_prowlarr_download(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    let cfg = app.prowlarr.resolve(app.pesto_config.as_ref());
    let Some(cfg) = cfg else {
        app.status_bar.set("Prowlarr not configured");
        return;
    };

    let nzb_dir = app
        .pesto_config
        .as_ref()
        .and_then(|c| c.nzb_dir.as_deref())
        .map(app::expand_tilde);
    let Some(nzb_dir) = nzb_dir else {
        app.status_bar
            .set("nzb_dir not configured — set it in pesto.toml");
        return;
    };

    let search = match app.prowlarr.search.as_mut() {
        Some(s) => s,
        None => return,
    };

    let result = match search.selected_result() {
        Some(r) => r.clone(),
        None => return,
    };

    let dest = prowlarr::dest_path_in(&nzb_dir, &result);
    search.downloading = true;
    app.status_bar.set(format!(
        "Downloading {}…",
        prowlarr::nzb_filename_for(&result)
    ));

    tokio::spawn(async move {
        let outcome = match prowlarr::build_client() {
            Ok(client) => prowlarr::download_nzb(&cfg, &client, &result, &dest)
                .await
                .map(|()| dest)
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(AppEvent::ProwlarrDownloadDone(outcome));
    });
}
