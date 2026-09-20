use std::{io, time::Duration};

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

mod app;
mod catalog;
mod events;
mod nzb_viewer;
mod prowlarr;
mod tasks;
mod ui;

use app::App;
use events::AppEvent;

#[tokio::main]
async fn main() -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // App::new() opens the SQLite catalog, imports legacy JSONL, and queries
    // history — all blocking I/O. Run it on the blocking thread pool so the
    // async runtime (and therefore the terminal) stays responsive.
    let mut app = tokio::task::spawn_blocking(App::new)
        .await
        .expect("App::new panicked");
    app.load_upload_prefs();
    app.load_queue();

    let upload_log_path = crate::catalog::default_log_path();

    // Initialise pesto's tracing subscriber to write DEBUG-level session logs.
    // No stderr output — the TUI owns the terminal. The writer is redirected to
    // a per-upload file via set_session_log() inside run_real_upload().
    let _ = pesto::logging::init_for_tui();

    // Event channel (the backbone of the new architecture)
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

    // NOTE: The old fake progress simulator was removed.
    // Real progress now only comes from actual `pesto::post()` calls.

    // Spawn keyboard event task using EventStream (async crossterm).
    //
    // IMPORTANT: keep reading on *every* event kind. A `while let Some(Ok(
    // Event::Key(_)))` would end the task on the first non-key event (a Resize
    // or FocusGained — both common right at startup inside tmux), silently
    // killing all keyboard input while the UI keeps redrawing. Match instead and
    // ignore the events we don't care about so the reader survives.
    let tx_keys = tx.clone();
    tokio::spawn(async move {
        let mut reader = EventStream::new();
        loop {
            match reader.next().await {
                Some(Ok(Event::Key(key))) => {
                    if key.kind == KeyEventKind::Press {
                        let _ = tx_keys.send(AppEvent::Key(key));
                    }
                }
                // Resize / Mouse / Focus / Paste: not handled, but must not stop
                // the reader.
                Some(Ok(_)) => {}
                // A read error or end-of-stream (stdin closed): nothing left to
                // read, so end the task.
                Some(Err(_)) | None => break,
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

    let res = run_app(
        &mut terminal,
        &mut app,
        tx.clone(),
        &mut rx,
        upload_log_path.as_deref(),
    )
    .await;

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
    upload_log_path: Option<&std::path::Path>,
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
                            && !app.watch.editing
                            && !app.show_upload_confirm
                            && app.prowlarr.search.is_none()
                            && app.prowlarr.batch.is_none() =>
                    {
                        return Ok(())
                    }
                    KeyCode::Esc
                        if !app.log_panel.searching
                            && !app.history.searching
                            && app.history.nzb_viewer.is_none()
                            && !app.config_state.editing
                            && !app.watch.editing
                            && !app.show_upload_confirm
                            && app.prowlarr.search.is_none()
                            && app.prowlarr.batch.is_none() =>
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
                    // F1–F7: direct tab jump
                    KeyCode::F(1) => {
                        app.state = app::AppState::Dashboard;
                    }
                    KeyCode::F(2) => {
                        app.state = app::AppState::Queue;
                    }
                    KeyCode::F(3) => {
                        app.state = app::AppState::Browser;
                    }
                    KeyCode::F(4) => {
                        app.state = app::AppState::History;
                        app.refresh_history();
                    }
                    KeyCode::F(5) => {
                        app.state = app::AppState::NzbVault;
                        app.load_vault();
                    }
                    KeyCode::F(6) => {
                        app.state = app::AppState::Config;
                    }
                    KeyCode::F(7) => {
                        app.state = app::AppState::Watch;
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
                            app.save_upload_prefs();
                            app.confirm_close();
                            app.state = app::AppState::Dashboard;
                            tasks::upload::handle_upload_trigger(app, tx.clone());
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
                        // Right/l/Space: advance cycle/number/toggle fields
                        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                            app.confirm_field_increment();
                        }
                        // Left/h: step cycle/number/toggle fields backwards
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
                            tasks::prowlarr::trigger_prowlarr_download(app, tx.clone());
                        }
                        _ => {}
                    },
                    // ── Hook picker overlay (takes priority over screen keys) ──
                    _ if app.hook_picker.is_some() => match key.code {
                        KeyCode::Esc | KeyCode::Char('q') => {
                            // Esc first cancels a pending re-send confirmation,
                            // then (a second time) closes the picker.
                            match app.hook_picker.as_mut().and_then(|p| p.pending_confirm) {
                                Some(_) => {
                                    if let Some(ref mut p) = app.hook_picker {
                                        p.pending_confirm = None;
                                    }
                                    app.status_bar.set("Re-send cancelled");
                                }
                                None => {
                                    app.hook_picker = None;
                                    app.status_bar.set("Hook picker closed");
                                }
                            }
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if let Some(ref mut p) = app.hook_picker {
                                p.move_down();
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if let Some(ref mut p) = app.hook_picker {
                                p.move_up();
                            }
                        }
                        KeyCode::Enter => {
                            // Re-sending a hook that already succeeded for this
                            // release asks for confirmation first (one extra Enter).
                            let needs_confirm = app
                                .hook_picker
                                .as_ref()
                                .map(|p| {
                                    p.pending_confirm != Some(p.selected)
                                        && p.selected_hook()
                                            .map(|h| p.sent_at(h).is_some())
                                            .unwrap_or(false)
                                })
                                .unwrap_or(false);
                            if needs_confirm {
                                if let Some(ref mut p) = app.hook_picker {
                                    p.pending_confirm = Some(p.selected);
                                }
                                app.status_bar.set(
                                    "Already sent — press Enter again to re-send, Esc to cancel",
                                );
                            } else {
                                tasks::hooks::run_selected_hook(app, tx.clone());
                            }
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
                            // Space is the single selection action: queue/unqueue
                            // the item under the cursor (file or folder), then
                            // advance. The queue is the one source of truth.
                            app.toggle_queue_at_cursor();
                        }
                        KeyCode::Enter => {
                            // Enter is navigation only and never touches the
                            // queue: enter a directory, or (on a file) just keep
                            // the detail panel focused on it. Use Space to queue.
                            if let Some(selected) = app.file_tree.get_selected().cloned() {
                                if selected.is_dir() {
                                    app.file_tree.current_dir = selected;
                                    app.file_tree.refresh();
                                    app.file_tree.selected = 0;
                                } else {
                                    app.status_bar.set("Press Space to queue/unqueue this file");
                                }
                            }
                        }
                        KeyCode::Char('b') | KeyCode::Backspace | KeyCode::Left => {
                            app.file_tree.go_to_parent();
                        }
                        KeyCode::Char('u') => {
                            // The queue already reflects everything marked with
                            // Space, so just open the config panel.
                            if app.upload_queue.items.is_empty() {
                                app.status_bar
                                    .set("Queue is empty — mark files with Space first");
                            } else {
                                app.show_upload_confirm = true;
                            }
                        }
                        KeyCode::Char('n') => {
                            // Toggle "show only items without an NZB yet".
                            app.file_tree.toggle_filter_unbacked();
                            let (_, unbacked, _) = app.file_tree.summary();
                            if app.file_tree.filter_unbacked {
                                app.status_bar.set(format!(
                                    "Showing {} item(s) that still need uploading (n to show all)",
                                    unbacked
                                ));
                            } else {
                                app.status_bar.set("Showing all items");
                            }
                        }
                        KeyCode::Char('p') | KeyCode::Char('P') => {
                            tasks::prowlarr::trigger_prowlarr_search(app, tx.clone());
                        }
                        KeyCode::Char('r') => {
                            tasks::hooks::trigger_run_hooks(app);
                        }
                        _ => {}
                    },
                    // ── Queue screen keys (the home for queue management) ───
                    _ if app.state == app::AppState::Queue => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => app.upload_queue.select_previous(),
                        KeyCode::Down | KeyCode::Char('j') => app.upload_queue.select_next(),
                        KeyCode::Char('u') => {
                            if app.upload_queue.items.is_empty() {
                                app.status_bar.set(
                                    "Queue is empty — go to Browser (F3) and mark files with Space",
                                );
                            } else if app.upload_in_progress {
                                app.status_bar.set("Upload already running");
                            } else {
                                app.show_upload_confirm = true;
                            }
                        }
                        KeyCode::Char('d') | KeyCode::Delete => {
                            if app.upload_in_progress {
                                app.status_bar.set("Cannot edit the queue during upload");
                            } else if let Some(removed) = app.remove_queue_selected() {
                                app.status_bar.set(format!("Removed: {}", removed));
                            }
                        }
                        KeyCode::Char('c') => {
                            if app.upload_in_progress {
                                app.status_bar.set("Cannot edit the queue during upload");
                            } else {
                                let count = app.clear_queue();
                                app.status_bar
                                    .set(format!("Cleared {} items from queue", count));
                            }
                        }
                        KeyCode::Char('J') if !app.upload_in_progress => {
                            app.upload_queue.move_selected_down();
                            app.save_queue();
                        }
                        KeyCode::Char('K') if !app.upload_in_progress => {
                            app.upload_queue.move_selected_up();
                            app.save_queue();
                        }
                        KeyCode::Char('x') if app.upload_in_progress => {
                            app.cancel_upload();
                        }
                        // Search Prowlarr for every queued release and auto-fetch
                        // exact-name matches directly into nzb_dir.
                        KeyCode::Char('p') | KeyCode::Char('P') => {
                            tasks::prowlarr::trigger_prowlarr_queue_search(app, tx.clone());
                        }
                        _ => {}
                    },
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
                    // Cancel current upload (Dashboard shows the live progress)
                    KeyCode::Char('x')
                        if app.state == app::AppState::Dashboard && app.upload_in_progress =>
                    {
                        app.cancel_upload();
                    }
                    // Pause/resume current upload
                    KeyCode::Char('p')
                        if app.state == app::AppState::Dashboard && app.upload_in_progress =>
                    {
                        app.toggle_pause_upload();
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
                        KeyCode::Char('p') | KeyCode::Char('P') => {
                            tasks::prowlarr::trigger_prowlarr_search(app, tx.clone());
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
                            tasks::prowlarr::trigger_prowlarr_check(app, tx.clone());
                        }
                        _ => {}
                    },
                    // ── Watch screen (text-edit mode) ──────────────────────
                    _ if app.state == app::AppState::Watch && app.watch.editing => match key.code {
                        KeyCode::Esc => app.watch_cancel_edit(),
                        KeyCode::Enter => app.watch_confirm_edit(),
                        KeyCode::Backspace => {
                            app.watch.edit_buf.pop();
                        }
                        KeyCode::Char(c) => {
                            app.watch.edit_buf.push(c);
                        }
                        _ => {}
                    },
                    // ── Watch screen (navigation) ──────────────────────────
                    _ if app.state == app::AppState::Watch => match key.code {
                        KeyCode::Char('j') | KeyCode::Down => app.watch_select_next(),
                        KeyCode::Char('k') | KeyCode::Up => app.watch_select_prev(),
                        KeyCode::Enter | KeyCode::Char('e') => app.watch_start_edit(),
                        KeyCode::Char('w') => app.toggle_watch_enabled(),
                        _ => {}
                    },
                    _ => {}
                },
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
                    app.record_catalog_entry(
                        original_name,
                        size_bytes,
                        nzb_path,
                        duration_s,
                        false,
                    );
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
                    app.watch_upload_done(
                        path, success, cancelled, size_bytes, nzb_path, duration_s,
                    );
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

        // Small sleep to avoid busy-looping the draw thread
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}
