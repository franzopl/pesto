use std::{
    io, path::PathBuf, sync::atomic::AtomicBool, sync::atomic::Ordering, sync::Arc, time::Duration,
};

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use pesto::{
    config::{Config, ObfuscateMode},
    walk::InputFile,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod app;
mod catalog;
mod events;
mod hooks;
mod nzb_viewer;
mod prowlarr;
mod ui;

use app::App;
use events::{AppEvent, FileProgressUpdate, ProgressUpdate};

#[tokio::main]
async fn main() -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();

    // Event channel (the backbone of the new architecture)
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    // NOTE: The old fake progress simulator was removed.
    // Real progress now only comes from actual `pesto::post()` calls.

    // Spawn keyboard event task using EventStream (async crossterm)
    let tx_keys = tx.clone();
    tokio::spawn(async move {
        let mut reader = EventStream::new();
        while let Some(Ok(Event::Key(key))) = reader.next().await {
            if key.kind == KeyEventKind::Press {
                let _ = tx_keys.send(AppEvent::Key(key));
            }
        }
    });

    // Also send periodic Tick events so the UI stays fresh
    let tx_tick = tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(120)).await;
            let _ = tx_tick.send(AppEvent::Tick);
        }
    });

    let res = run_app(&mut terminal, &mut app, tx.clone(), &mut rx).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("Error: {:?}", err);
    }

    Ok(())
}

async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    tx: mpsc::UnboundedSender<AppEvent>,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Drain all pending events (non-blocking)
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::Key(key) => match key.code {
                    KeyCode::Char('q')
                        if !app.log_panel.searching
                            && !app.history.searching
                            && app.history.nzb_viewer.is_none()
                            && !app.config_state.editing
                            && !app.show_upload_confirm =>
                    {
                        return Ok(())
                    }
                    KeyCode::Esc
                        if !app.log_panel.searching
                            && !app.history.searching
                            && app.history.nzb_viewer.is_none()
                            && !app.config_state.editing
                            && !app.show_upload_confirm =>
                    {
                        return Ok(())
                    }
                    KeyCode::Tab => {
                        app.next_tab();
                        if app.state == app::AppState::History {
                            app.refresh_history();
                        }
                    }
                    KeyCode::BackTab => {
                        app.prev_tab();
                        if app.state == app::AppState::History {
                            app.refresh_history();
                        }
                    }
                    // F1–F5: direct tab jump
                    KeyCode::F(1) => {
                        app.state = app::AppState::Dashboard;
                    }
                    KeyCode::F(2) => {
                        app.state = app::AppState::Browser;
                    }
                    KeyCode::F(3) => {
                        app.state = app::AppState::History;
                        app.refresh_history();
                    }
                    KeyCode::F(4) => {
                        app.state = app::AppState::NzbVault;
                        app.load_vault();
                    }
                    KeyCode::F(5) => {
                        app.state = app::AppState::Config;
                    }
                    // ── Upload config panel (text-edit mode takes priority) ──
                    _ if app.show_upload_confirm && app.confirm_editing => match key.code {
                        KeyCode::Esc => app.confirm_cancel_edit(),
                        KeyCode::Enter => app.confirm_confirm_edit(),
                        KeyCode::Backspace => {
                            app.confirm_edit_buf.pop();
                        }
                        KeyCode::Tab => app.confirm_toggle_password_reveal(),
                        KeyCode::Char(c) => app.confirm_edit_buf.push(c),
                        _ => {}
                    },
                    // ── Upload config panel (navigation mode) ─────────────
                    _ if app.show_upload_confirm => match key.code {
                        // y or Ctrl+Enter = start upload
                        KeyCode::Char('y') => {
                            app.confirm_close();
                            app.state = app::AppState::Dashboard;
                            handle_upload_trigger(app, tx.clone());
                        }
                        // Esc/n = cancel panel (stay in browser)
                        KeyCode::Esc | KeyCode::Char('n') => {
                            app.confirm_close();
                            app.status_bar.set("Upload cancelled");
                        }
                        KeyCode::Down | KeyCode::Char('j') => app.confirm_field_next(),
                        KeyCode::Up | KeyCode::Char('k') => app.confirm_field_prev(),
                        // Enter or e: cycle enum/bool, or enter edit mode for text fields
                        KeyCode::Enter | KeyCode::Char('e') => app.confirm_field_activate(),
                        // Right/l/Space: increment cycle fields
                        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                            app.confirm_field_increment();
                        }
                        // Left/h: decrement (PAR2 only)
                        KeyCode::Left | KeyCode::Char('h') => app.confirm_field_decrement(),
                        _ => {}
                    },
                    // ── Prowlarr search overlay (takes priority over all screens) ──
                    _ if app.prowlarr.search.is_some() => match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            app.prowlarr.search = None;
                            app.status_bar.set("Search closed");
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if let Some(ref mut s) = app.prowlarr.search {
                                s.move_down();
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if let Some(ref mut s) = app.prowlarr.search {
                                s.move_up();
                            }
                        }
                        KeyCode::Char('d') => {
                            trigger_prowlarr_download(app, tx.clone());
                        }
                        _ => {}
                    },
                    KeyCode::Char('h') if app.state == app::AppState::Browser => {
                        app.file_tree.toggle_hidden();
                    }
                    _ if app.state == app::AppState::Browser => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => app.file_tree.select_previous(),
                        KeyCode::Down | KeyCode::Char('j') => app.file_tree.select_next(),
                        KeyCode::Char(' ') => {
                            // Space: mark/unmark current item and advance cursor
                            app.file_tree.toggle_mark();
                            let n = app.file_tree.marked_count();
                            app.status_bar
                                .set(format!("{} item(s) marked — press u to queue & upload", n));
                        }
                        KeyCode::Enter => {
                            if let Some(selected) = app.file_tree.get_selected().cloned() {
                                if selected.is_dir() {
                                    app.file_tree.current_dir = selected;
                                    app.file_tree.refresh();
                                    app.file_tree.selected = 0;
                                } else {
                                    let path_str = selected.to_string_lossy().to_string();
                                    if app.upload_queue.items.contains(&path_str) {
                                        app.upload_queue.items.retain(|p| p != &path_str);
                                        app.status_bar.set("Removed from queue");
                                        app.log_panel.push(format!("Removed: {}", path_str));
                                    } else {
                                        app.add_to_queue(path_str);
                                    }
                                }
                            }
                        }
                        KeyCode::Char('b') | KeyCode::Backspace | KeyCode::Left => {
                            app.file_tree.go_to_parent();
                        }
                        KeyCode::Char('u') => {
                            // Queue all marked items first
                            let marked = app.file_tree.take_marked();
                            for p in marked {
                                app.add_to_queue(p.to_string_lossy().to_string());
                            }
                            if app.upload_queue.items.is_empty() {
                                app.status_bar
                                    .set("Queue is empty — mark files with Space first");
                            } else {
                                app.show_upload_confirm = true;
                            }
                        }
                        KeyCode::Char('P') => {
                            trigger_prowlarr_search(app, tx.clone());
                        }
                        _ => {}
                    },
                    KeyCode::Char('u')
                        if app.state == app::AppState::Dashboard && !app.log_panel.searching =>
                    {
                        if app.upload_queue.items.is_empty() {
                            app.status_bar
                                .set("Queue is empty — go to Browser and mark files with Space");
                        } else {
                            app.show_upload_confirm = true;
                        }
                    }
                    // ── Log panel search (Dashboard) ───────────────────────
                    _ if app.state == app::AppState::Dashboard && app.log_panel.searching => {
                        match key.code {
                            KeyCode::Esc => app.log_panel.search_clear(),
                            KeyCode::Enter => app.log_panel.search_confirm(),
                            KeyCode::Backspace => app.log_panel.search_pop(),
                            KeyCode::Char(c) => app.log_panel.search_push(c),
                            _ => {}
                        }
                    }
                    KeyCode::Char('/') if app.state == app::AppState::Dashboard => {
                        app.log_panel.start_search();
                    }
                    // Log scrolling when on Dashboard
                    KeyCode::Up | KeyCode::Char('k') if app.state == app::AppState::Dashboard => {
                        app.log_panel.scroll_up(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if app.state == app::AppState::Dashboard => {
                        app.log_panel.scroll_down(1);
                    }
                    KeyCode::PageUp if app.state == app::AppState::Dashboard => {
                        app.log_panel.scroll_up(10);
                    }
                    KeyCode::PageDown if app.state == app::AppState::Dashboard => {
                        app.log_panel.scroll_down(10);
                    }
                    KeyCode::Char('g') if app.state == app::AppState::Dashboard => {
                        app.log_panel.scroll_to_top();
                    }
                    KeyCode::Char('G') if app.state == app::AppState::Dashboard => {
                        app.log_panel.scroll_to_bottom();
                    }
                    KeyCode::Char('a')
                        if app.state == app::AppState::Dashboard && !app.log_panel.searching =>
                    {
                        app.log_panel.toggle_auto_scroll();
                    }
                    // Queue management on Dashboard
                    KeyCode::Char('d') | KeyCode::Delete
                        if app.state == app::AppState::Dashboard =>
                    {
                        if let Some(removed) = app.upload_queue.remove_selected() {
                            app.status_bar.set(format!("Removed: {}", removed));
                            app.log_panel
                                .push(format!("Removed from queue: {}", removed));
                            if app.upload_queue.items.is_empty() {
                                app.upload_in_progress = false;
                            }
                        }
                    }
                    KeyCode::Char('c') if app.state == app::AppState::Dashboard => {
                        let count = app.upload_queue.items.len();
                        app.upload_queue.clear();
                        app.status_bar
                            .set(format!("Cleared {} items from queue", count));
                        app.log_panel.push("Upload queue cleared".to_string());
                        app.upload_in_progress = false;
                    }
                    // Cancel current upload
                    KeyCode::Char('x')
                        if app.state == app::AppState::Dashboard && app.upload_in_progress =>
                    {
                        app.cancel_upload();
                    }
                    // Pause / Resume
                    KeyCode::Char('p')
                        if app.state == app::AppState::Dashboard && app.upload_in_progress =>
                    {
                        app.toggle_pause();
                        // Send event so the upload task can react (for now UI-only pause)
                        let _ = tx.send(AppEvent::PauseUpload); // will be improved
                    }
                    // Navigate and reorder queue on Dashboard (Shift+J/K = move item)
                    KeyCode::Char('J')
                        if app.state == app::AppState::Dashboard && !app.upload_in_progress =>
                    {
                        app.upload_queue.move_selected_down();
                        app.status_bar.set("Moved item down");
                    }
                    KeyCode::Char('K')
                        if app.state == app::AppState::Dashboard && !app.upload_in_progress =>
                    {
                        app.upload_queue.move_selected_up();
                        app.status_bar.set("Moved item up");
                    }
                    // Navigate queue selection (no reorder) during upload
                    KeyCode::Char('J') if app.state == app::AppState::Dashboard => {
                        app.upload_queue.select_next();
                    }
                    KeyCode::Char('K') if app.state == app::AppState::Dashboard => {
                        app.upload_queue.select_previous();
                    }
                    // ── History screen keys ────────────────────────────────
                    // NZB viewer overlay takes priority when open
                    _ if app.state == app::AppState::History
                        && app.history.nzb_viewer.is_some() =>
                    {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => app.close_nzb_viewer(),
                            KeyCode::Char('j') | KeyCode::Down => app.nzb_viewer_scroll_down(),
                            KeyCode::Char('k') | KeyCode::Up => app.nzb_viewer_scroll_up(),
                            _ => {}
                        }
                    }
                    _ if app.state == app::AppState::History && !app.history.searching => {
                        match key.code {
                            KeyCode::Char('j') | KeyCode::Down => app.history_select_next(),
                            KeyCode::Char('k') | KeyCode::Up => app.history_select_prev(),
                            KeyCode::Enter => app.open_nzb_viewer(),
                            KeyCode::Char('s') => {
                                app.history.show_stats = !app.history.show_stats;
                                if app.history.show_stats {
                                    app.refresh_stats();
                                }
                            }
                            KeyCode::Char('/') => {
                                app.history.searching = true;
                            }
                            _ => {}
                        }
                    }
                    _ if app.state == app::AppState::History && app.history.searching => {
                        match key.code {
                            KeyCode::Esc => {
                                app.history.searching = false;
                                app.history.query.clear();
                                app.refresh_history();
                            }
                            KeyCode::Enter => {
                                app.history.searching = false;
                                app.refresh_history();
                            }
                            KeyCode::Backspace => {
                                app.history.query.pop();
                                app.refresh_history();
                            }
                            KeyCode::Char(c) => {
                                app.history.query.push(c);
                                app.refresh_history();
                            }
                            _ => {}
                        }
                    }
                    // ── Config screen keys ────────────────────────────────
                    // ── NZB Vault viewer overlay ─────────────────────────────
                    _ if app.state == app::AppState::NzbVault && app.vault.viewer.is_some() => {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => {
                                app.vault.viewer = None;
                            }
                            KeyCode::Char('j') | KeyCode::Down => {
                                if let Some(ref mut v) = app.vault.viewer {
                                    v.scroll = v.scroll.saturating_add(1);
                                }
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                if let Some(ref mut v) = app.vault.viewer {
                                    v.scroll = v.scroll.saturating_sub(1);
                                }
                            }
                            _ => {}
                        }
                    }
                    // ── NZB Vault list ────────────────────────────────────────
                    _ if app.state == app::AppState::NzbVault => match key.code {
                        KeyCode::Char('j') | KeyCode::Down => app.vault.move_down(),
                        KeyCode::Char('k') | KeyCode::Up => app.vault.move_up(),
                        KeyCode::Enter => {
                            app.vault_parse_selected();
                        }
                        KeyCode::Char('v') => {
                            app.vault_open_viewer();
                        }
                        KeyCode::Char('s') => {
                            app.vault.cycle_sort();
                            app.status_bar.set(format!("Sort: {:?}", app.vault.sort));
                        }
                        KeyCode::Char('r') => {
                            app.load_vault();
                        }
                        KeyCode::Char('d') => {
                            if let Some(entry) = app.vault.selected_entry() {
                                let path = entry.path.clone();
                                match std::fs::remove_file(&path) {
                                    Ok(()) => {
                                        app.status_bar.set(format!(
                                            "Deleted {}",
                                            path.file_name().unwrap_or_default().to_string_lossy()
                                        ));
                                        app.load_vault();
                                    }
                                    Err(e) => {
                                        app.status_bar.set(format!("Delete failed: {}", e));
                                    }
                                }
                            }
                        }
                        KeyCode::Char('P') => {
                            trigger_prowlarr_search(app, tx.clone());
                        }
                        _ => {}
                    },
                    _ if app.state == app::AppState::Config && app.config_state.editing => {
                        match key.code {
                            KeyCode::Esc => app.config_cancel_edit(),
                            KeyCode::Enter => app.config_confirm_edit(),
                            KeyCode::Backspace => {
                                app.config_state.edit_buf.pop();
                            }
                            KeyCode::Char(c) => {
                                app.config_state.edit_buf.push(c);
                            }
                            _ => {}
                        }
                    }
                    _ if app.state == app::AppState::Config => match key.code {
                        KeyCode::Char('j') | KeyCode::Down => app.config_select_next(),
                        KeyCode::Char('k') | KeyCode::Up => app.config_select_prev(),
                        KeyCode::Enter | KeyCode::Char('e') => app.config_start_edit(),
                        KeyCode::Char('r') => app.config_reset_field(),
                        KeyCode::Char('R') => app.config_reset_all(),
                        KeyCode::Char('C') => {
                            trigger_prowlarr_check(app, tx.clone());
                        }
                        _ => {}
                    },
                    _ => {}
                },
                AppEvent::Progress(msg) => {
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
                    app.log_panel.push_error(format!("ERROR: {}", msg));
                    app.status_bar.set("Upload error — see logs for details");
                }
                AppEvent::ProgressUpdate(update) if !app.upload_paused => {
                    app.handle_progress_update(update);
                }
                AppEvent::PauseUpload => {
                    // Currently pause is UI-driven; in future we can throttle the worker here
                }
                AppEvent::ResumeUpload => {}
                AppEvent::UploadFinished { success, cancelled } => {
                    app.upload_finished(success, cancelled);
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
                            if app.state == app::AppState::NzbVault {
                                app.load_vault();
                            }
                        }
                        Err(e) => {
                            app.status_bar.set(format!("Download failed: {}", e));
                        }
                    }
                }
                AppEvent::Tick => {
                    // could do animations, throughput calc, etc. here later
                }
                _ => {}
            }
        }

        // Small sleep to avoid busy-looping the draw thread
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

/// Called when the user presses 'C' on the Config screen.
/// Spawns an async task that tests the Prowlarr connection and sends the result back.
fn trigger_prowlarr_check(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
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
fn trigger_prowlarr_search(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    use app::ProwlarrSearchState;

    let cfg = app.prowlarr.resolve(app.pesto_config.as_ref());
    let Some(cfg) = cfg else {
        app.status_bar
            .set("Prowlarr not configured — set URL and API key in Config (F5)");
        return;
    };

    // Derive the release name from the selected path (Browser or Vault).
    let filename: Option<String> = match app.state {
        app::AppState::Browser => app
            .file_tree
            .get_selected()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
        app::AppState::NzbVault => app.vault.selected_entry().map(|e| e.name.clone()),
        _ => None,
    };

    let Some(filename) = filename else {
        app.status_bar.set("Nothing selected to search");
        return;
    };

    // Strip the file extension to get the release name.
    let release_name = prowlarr::release_name_from_filename(&filename).to_string();

    app.status_bar
        .set(format!("Searching Prowlarr for \"{}\"…", release_name));
    app.prowlarr.search = Some(ProwlarrSearchState::new(release_name.clone()));

    tokio::spawn(async move {
        let result = match prowlarr::build_client() {
            Ok(client) => prowlarr::search_by_release(&cfg, &client, &release_name)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(AppEvent::ProwlarrSearchDone(result));
    });
}

/// Called when the user presses 'd' on a search result to download its NZB.
fn trigger_prowlarr_download(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    let cfg = app.prowlarr.resolve(app.pesto_config.as_ref());
    let Some(cfg) = cfg else {
        app.status_bar.set("Prowlarr not configured");
        return;
    };

    let nzb_dir = app
        .pesto_config
        .as_ref()
        .and_then(|c| c.nzb_dir.as_deref())
        .map(PathBuf::from);
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

/// Called when the user presses 'u' on the Dashboard.
/// Starts a real (dry-run) upload using pesto::post() and streams progress.
fn handle_upload_trigger(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    app.trigger_upload();

    // Snapshot the files currently in the queue
    let files: Vec<PathBuf> = app.upload_queue.items.iter().map(PathBuf::from).collect();

    if files.is_empty() {
        return;
    }

    // Use real config (with session overrides applied) or fall back to dry-run
    let config = if let Some(mut real_cfg) = app.effective_config_with_overrides() {
        real_cfg.dry_run = false;
        real_cfg
    } else {
        build_dry_run_config()
    };

    // Collect file names for the hook context (display names, not full paths)
    let queue_names: Vec<String> = files
        .iter()
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .collect();

    // Expand directories recursively into individual InputFiles
    let input_files = match pesto::walk::expand_inputs(&files) {
        Ok(v) => v,
        Err(e) => {
            app.log_panel
                .push_error(format!("ERROR expanding inputs: {}", e));
            app.status_bar.set("Input error — see logs");
            app.upload_in_progress = false;
            return;
        }
    };

    // Token is already created and stored in app.current_cancel_token by trigger_upload
    let cancel_token = app.current_cancel_token.clone().unwrap_or_default();

    // Spawn the actual upload task
    tokio::spawn(async move {
        let tx2 = tx.clone();
        let result = run_real_upload(config.clone(), input_files, tx.clone(), cancel_token).await;

        let success = result.is_ok();
        let cancelled = result.as_ref().is_ok_and(|r| r.cancelled);

        // Run post-upload hooks on successful, non-cancelled upload
        if success && !cancelled {
            let total_bytes = result.as_ref().map(|r| r.total_bytes).unwrap_or(0);
            let ctx = hooks::HookContext::from_config(&config, &queue_names, total_bytes, None);
            let hook_cfg = config.clone();
            let log_lines = tokio::task::spawn_blocking(move || hooks::run_hooks(&hook_cfg, &ctx))
                .await
                .unwrap_or_else(|e| vec![format!("hook task error: {e}")]);
            for line in log_lines {
                let _ = tx2.send(AppEvent::Progress(format!("[hook] {}", line)));
            }
        }

        if let Err(ref e) = result {
            if !e.is_cancelled() {
                let _ = tx2.send(AppEvent::UploadError(e.to_string()));
            }
        }
        let _ = tx2.send(AppEvent::UploadFinished {
            success: success && result.is_ok(),
            cancelled,
        });
    });
}

/// Constructs a minimal Config that exercises the full hot path in dry-run mode.
fn build_dry_run_config() -> Config {
    Config {
        host: "dry-run.local".into(),
        port: 563,
        ssl: true,
        connections: 2,
        username: None,
        password: None,
        retry_delay: 1,
        extra_servers: vec![],
        from: "upapasta@local".into(),
        groups: vec!["alt.binaries.test".into()],
        article_size: 768_000,
        line_length: 128,
        retries: 2,
        obfuscate: ObfuscateMode::None,
        date: None,
        no_archive: true,
        message_id_domain: None,
        dry_run: true, // ← never touches the network
        par2: 5,
        par2_memory_limit: None,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_only: false,
        threads: 0,
        simd: parmesan::SimdPath::Auto,
        verify: false,
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        indexer_url: None,
        indexer_api_key: None,
        indexer_category: None,
        nzb_dir: None,
        no_upload: false,
        history: false,
        history_dir: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        post_hook: None,
        no_hooks: true,
        nfo: false,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 0,
        check_retries: 1,
        pipeline_depth: 0,
    }
}

/// The actual async upload worker. Forwards every ProgressEvent into the UI in real time.
/// Supports cancellation via CancellationToken (bridged to pesto's AtomicBool).
async fn run_real_upload(
    config: Config,
    files: Vec<InputFile>,
    tx: mpsc::UnboundedSender<AppEvent>,
    cancel_token: CancellationToken,
) -> Result<UploadResult, UploadError> {
    // Bridge: spawn a task that sets the AtomicBool when the token fires.
    let cancel_flag = Arc::new(AtomicBool::new(false));
    {
        let flag = cancel_flag.clone();
        let token = cancel_token.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            flag.store(true, Ordering::Relaxed);
        });
    }

    // Create our own progress channel so we can drain events in real time
    // while the poster runs concurrently (pesto::post_cancelable awaits completion
    // before returning, so all events would arrive only after the upload finishes).
    let (prog_tx, mut prog_rx) =
        tokio::sync::mpsc::unbounded_channel::<pesto::progress::ProgressEvent>();

    // Spawn the poster as a concurrent task
    let cfg2 = config.clone();
    let files2 = files.clone();
    let cf2 = cancel_flag.clone();
    let upload_handle = tokio::spawn(async move {
        pesto::poster::post_files_with_progress_and_cancel(
            &cfg2,
            &files2,
            Some(prog_tx),
            None,
            Some(cf2),
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
    };

    // Drain progress events as they arrive (real-time, not post-hoc).
    //
    // select! races the upload task against the event channel so that if the
    // task exits without emitting Finished (panic, early return) we still
    // unblock. When a terminal event (Finished/Interrupted/Failed) arrives on
    // the channel we disable the event arm; the task arm fires on the next
    // iteration and we collect the PostOutcome.
    tokio::pin!(upload_handle);
    let mut events_done = false;

    let outcome = loop {
        tokio::select! {
            // Upload task completed — drain any buffered events then stop.
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
                    Ok(Err(e)) => return Err(UploadError::Pesto(e.to_string())),
                    Err(e) => return Err(UploadError::Pesto(format!("upload task panicked: {e}"))),
                };
            }
            // Progress event — forward to UI (disabled once terminal event seen).
            event = prog_rx.recv(), if !events_done => {
                let Some(event) = event else {
                    // Channel closed without Finished — let the task arm resolve.
                    events_done = true;
                    continue;
                };
                let msg = format_progress_event(&event);
                if !msg.is_empty() {
                    let _ = tx.send(AppEvent::Progress(msg));
                }
                if let Some(update) = extract_progress_update(&event, &last_update) {
                    last_update = update.clone();
                    let _ = tx.send(AppEvent::ProgressUpdate(update));
                }
                // On any terminal event, disable this arm so the task arm fires next.
                if matches!(
                    event,
                    pesto::progress::ProgressEvent::Finished
                        | pesto::progress::ProgressEvent::Interrupted
                        | pesto::progress::ProgressEvent::Failed { .. }
                ) {
                    events_done = true;
                }
            }
        }
    };

    let _ = tx.send(AppEvent::Progress(format!(
        "PostOutcome: {} segments, {} failures",
        outcome.segments.len(),
        outcome.failures.len()
    )));

    let cancelled = outcome.cancelled;
    let total_bytes = last_update.total_bytes;
    Ok(UploadResult {
        cancelled,
        total_bytes,
    })
}

#[derive(Debug)]
struct UploadResult {
    cancelled: bool,
    total_bytes: u64,
}

#[derive(Debug)]
enum UploadError {
    Pesto(String),
}

impl UploadError {
    fn is_cancelled(&self) -> bool {
        false
    }
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UploadError::Pesto(s) => write!(f, "{}", s),
        }
    }
}

/// Convert rich pesto ProgressEvent into a single-line string for the log.
fn format_progress_event(ev: &pesto::progress::ProgressEvent) -> String {
    use pesto::progress::ProgressEvent as E;

    match ev {
        E::Started {
            files,
            connections,
            mode,
            ..
        } => {
            format!(
                "Started ({:?}) — {} files, {} connections",
                mode,
                files.len(),
                connections
            )
        }
        E::SegmentDone { file, ok, .. } if !ok => {
            format!("Segment FAILED — {}", file)
        }
        E::SegmentDone { .. } => String::new(), // shown in gauge, not logs
        E::Status { text } if !text.is_empty() => format!("Status: {}", text),
        E::QueueExtended { file, segments, .. } => {
            format!("PAR2 extended queue: {} (+{} segments)", file, segments)
        }
        E::Par2EncodeStarted {
            input_files,
            recovery_slices,
            ..
        } => {
            format!(
                "PAR2 encoding started — {} files, {} recovery slices",
                input_files, recovery_slices
            )
        }
        E::Par2InputProgress { .. } => String::new(), // shown in gauge, not logs
        E::Par2SliceWritten => String::new(),         // too noisy
        E::Finished => "=== Pesto run finished ===".into(),
        E::Failed { description } => format!("FAILED: {}", description),
        E::Interrupted => "Interrupted by user".into(),
        _ => String::new(), // many low-level events are too noisy for the TUI log
    }
}

/// Extract accurate numbers from ProgressEvent for the progress bar + per-file updates
fn extract_progress_update(
    ev: &pesto::progress::ProgressEvent,
    previous: &ProgressUpdate,
) -> Option<ProgressUpdate> {
    use crate::events::UploadPhase;
    use pesto::progress::ProgressEvent as E;

    match ev {
        E::Started {
            files,
            par2_bytes_hint,
            ..
        } => {
            let total_segments: u64 = files.iter().map(|f| f.segments).sum();
            let total_bytes: u64 = files.iter().map(|f| f.bytes).sum::<u64>() + par2_bytes_hint;
            Some(ProgressUpdate {
                done_segments: 0,
                total_segments,
                done_bytes: 0,
                total_bytes,
                current_speed_mbps: 0.0,
                message: None,
                file_update: None,
                phase: Some(UploadPhase::Uploading),
                par2_slices: None,
            })
        }
        E::CompressStarted { total_bytes } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Compressing {
                done_bytes: 0,
                total_bytes: *total_bytes,
            }),
            par2_slices: None,
        }),
        E::CompressProgress { bytes_written } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Compressing {
                done_bytes: *bytes_written,
                total_bytes: match &previous.phase {
                    Some(UploadPhase::Compressing { total_bytes, .. }) => *total_bytes,
                    _ => 0,
                },
            }),
            par2_slices: None,
        }),
        E::CompressDone => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Preparing),
            par2_slices: None,
        }),
        // Par2EncodeStarted is a config announcement, NOT a sequential phase.
        // PAR2 encoding runs concurrently with NNTP posting. Store total slices
        // for the concurrent progress indicator; keep the phase as Uploading.
        E::Par2EncodeStarted {
            recovery_slices, ..
        } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Uploading),
            par2_slices: Some((0, *recovery_slices)),
        }),
        E::Par2InputProgress { done, total } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: None, // phase stays Uploading
            par2_slices: Some((*done, *total)),
        }),
        // PAR2 volumes are written to disk after encoding completes (sequential).
        E::Par2WriteStarted { total } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::WritingPar2 {
                written: 0,
                total: *total,
            }),
            par2_slices: None,
        }),
        E::Par2SliceWritten => {
            let (written, total) = match &previous.phase {
                Some(UploadPhase::WritingPar2 { written, total }) => (written + 1, *total),
                _ => (1, 1),
            };
            Some(ProgressUpdate {
                done_segments: previous.done_segments,
                total_segments: previous.total_segments,
                done_bytes: previous.done_bytes,
                total_bytes: previous.total_bytes,
                current_speed_mbps: previous.current_speed_mbps,
                message: None,
                file_update: None,
                phase: Some(UploadPhase::WritingPar2 { written, total }),
                par2_slices: None,
            })
        }
        E::CheckStarted { total } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Verifying {
                checked: 0,
                total: *total,
            }),
            par2_slices: None,
        }),
        E::CheckProgress { checked, .. } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Verifying {
                checked: *checked,
                total: match &previous.phase {
                    Some(UploadPhase::Verifying { total, .. }) => *total,
                    _ => 0,
                },
            }),
            par2_slices: None,
        }),
        E::SegmentDone { file, bytes, ok } => {
            let file_update = FileProgressUpdate {
                name: file.clone(),
                done_segments: 1,
                total_segments: 0,
                done_bytes: *bytes,
                total_bytes: 0,
                ok: *ok,
            };
            Some(ProgressUpdate {
                done_segments: previous.done_segments + 1,
                total_segments: previous.total_segments,
                done_bytes: previous.done_bytes + bytes,
                total_bytes: previous.total_bytes,
                current_speed_mbps: previous.current_speed_mbps,
                message: None,
                file_update: Some(file_update),
                phase: None,
                par2_slices: None,
            })
        }
        E::QueueExtended {
            segments, bytes, ..
        } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments + segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes + bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: None,
            par2_slices: None,
        }),
        _ => None,
    }
}
