use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    sync::atomic::Ordering,
    sync::Arc,
    time::Duration,
    time::Instant,
};

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use pesto::config::{Config, ObfuscateMode};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod app;
mod catalog;
mod events;
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

    // App::new() opens the SQLite catalog, imports legacy JSONL, and queries
    // history — all blocking I/O. Run it on the blocking thread pool so the
    // async runtime (and therefore the terminal) stays responsive.
    let mut app = tokio::task::spawn_blocking(App::new)
        .await
        .expect("App::new panicked");
    app.load_upload_prefs();
    app.load_queue();

    let upload_log_path = crate::catalog::default_log_path();

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
                    // F1–F6: direct tab jump
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
                            trigger_prowlarr_download(app, tx.clone());
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
                                run_selected_hook(app, tx.clone());
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
                            trigger_prowlarr_search(app, tx.clone());
                        }
                        KeyCode::Char('r') => {
                            trigger_run_hooks(app);
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
                            trigger_prowlarr_queue_search(app, tx.clone());
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
                    // Persist lines that are meaningful for post-session review.
                    if let Some(lp) = upload_log_path {
                        let m = msg.trim();
                        if m.starts_with("===")
                            || m.starts_with("wrote nzb")
                            || m.starts_with("wrote nfo")
                            || m.starts_with("PostOutcome")
                            || m.starts_with("FAILED")
                            || m.starts_with("Segment FAILED")
                        {
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

        // Small sleep to avoid busy-looping the draw thread
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

/// Called when the user presses 'r' on the Browser (or Vault): open the hook
/// picker so the user runs one chosen hook against the selected release.
///
/// Resolves the selected item to a release name (and a direct `.nzb` path when
/// the selection already is an NZB), lists the executable scripts in
/// `~/.config/pesto/hooks/`, and shows the picker. The actual run happens in
/// [`run_selected_hook`] once the user confirms.
fn trigger_run_hooks(app: &mut App) {
    if app.pesto_config.is_none() {
        app.status_bar
            .set("pesto.toml not loaded — needed to locate nzb_dir and hooks");
        return;
    }

    // Resolve the selected item to a release name and, when possible, a direct
    // `.nzb` path (Vault entries and `.nzb` files are already NZBs) plus the
    // media path on disk (so a `.nfo` can be generated when none exists yet).
    let (release_name, direct_nzb, media_path): (String, Option<PathBuf>, Option<PathBuf>) =
        match app.state {
            app::AppState::NzbVault => match app.vault.selected_entry() {
                Some(e) => (
                    prowlarr::release_name_from_filename(&e.name).to_string(),
                    Some(e.path.clone()),
                    None,
                ),
                None => {
                    app.status_bar.set("Nothing selected to run hooks on");
                    return;
                }
            },
            app::AppState::Browser => match app.file_tree.get_selected().cloned() {
                Some(p) => {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let is_nzb = p
                        .extension()
                        .map(|x| x.eq_ignore_ascii_case("nzb"))
                        .unwrap_or(false);
                    // A release folder has no extension — stripping one would drop
                    // a group tag like `.DUAL-Kallango`, breaking the release-key
                    // match. Only files carry a container/.nzb extension to strip.
                    let release_name = if p.is_dir() {
                        name.clone()
                    } else {
                        prowlarr::release_name_from_filename(&name).to_string()
                    };
                    // The media path is the selection itself, unless it already
                    // is the `.nzb` (then there's no media to mediainfo).
                    let media_path = (!is_nzb).then(|| p.clone());
                    (release_name, is_nzb.then_some(p), media_path)
                }
                None => {
                    app.status_bar.set("Nothing selected to run hooks on");
                    return;
                }
            },
            _ => {
                app.status_bar.set("Select a release in Browser or Vault");
                return;
            }
        };

    let hooks = pesto::hooks::list_hook_scripts();
    if hooks.is_empty() {
        app.status_bar
            .set("No executable hooks in ~/.config/pesto/hooks/");
        return;
    }

    // Past successful runs for this release, so the picker can flag what was
    // already sent and confirm before re-sending.
    let runs = {
        let key = ui::components::file_tree::release_key(&release_name);
        app.catalog
            .as_ref()
            .and_then(|c| c.hook_runs_for(&key).ok())
            .unwrap_or_default()
    };

    app.hook_picker = Some(app::HookPickerState::new(
        release_name,
        direct_nzb,
        media_path,
        hooks,
        runs,
    ));
}

/// Run the hook chosen in the picker against the selected release.
///
/// Closes the overlay, resolves the `.nzb` (a directly selected one wins;
/// otherwise the matching release in `nzb_dir` by release key), finds a sibling
/// `.nfo`, and runs exactly that one hook with the usual `PESTO_*` environment.
/// Runs off-thread; output streams back via [`AppEvent::HooksDone`].
fn run_selected_hook(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    let Some(picker) = app.hook_picker.take() else {
        return;
    };
    let Some(hook) = picker.selected_hook().cloned() else {
        app.status_bar.set("No hook selected");
        return;
    };
    let Some(cfg) = app.pesto_config.clone() else {
        app.status_bar.set("pesto.toml not loaded");
        return;
    };

    let nzb_dir = cfg.nzb_dir.as_deref().map(app::expand_tilde);
    let release_name = picker.release_name;
    let direct_nzb = picker.direct_nzb;
    let media_path = picker.media_path;
    let hook_name = hook
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| hook.display().to_string());

    app.status_bar
        .set(format!("Running hook {hook_name} for \"{release_name}\"…"));
    app.log_panel.push(format!(
        "=== Running hook {hook_name} for {release_name} ==="
    ));

    tokio::task::spawn_blocking(move || {
        // Locate the NZB: a directly selected one wins; otherwise search nzb_dir
        // by release key (so a media folder maps to its release/season .nzb).
        let nzb_path = direct_nzb.or_else(|| {
            nzb_dir
                .as_deref()
                .and_then(|d| app::find_nzb_for_release(d, &release_name))
        });
        let Some(nzb_path) = nzb_path else {
            let _ = tx.send(AppEvent::HooksDone {
                ok: false,
                release_key: String::new(),
                release_name: release_name.clone(),
                hook_name: hook_name.clone(),
                log: vec![format!("No .nzb found for \"{release_name}\" in nzb_dir")],
            });
            return;
        };

        // Resolve a `.nfo`: an existing sibling wins; otherwise generate one from
        // the local media via mediainfo and persist it next to the `.nzb` so the
        // hook (e.g. Curupira) gets PESTO_NFO. Best-effort — a missing mediainfo
        // or non-media selection just leaves PESTO_NFO empty as before.
        let mut nfo_log: Option<String> = None;
        let nfo_path = app::find_sibling_nfo(&nzb_path).or_else(|| {
            let media = media_path.as_ref()?;
            let content = pesto::nfo::generate(std::slice::from_ref(media))?;
            let dest = nzb_path.with_extension("nfo");
            match pesto::nfo::write(&dest, &content) {
                Ok(()) => {
                    nfo_log = Some(format!("generated .nfo via mediainfo: {}", dest.display()));
                    Some(dest)
                }
                Err(e) => {
                    nfo_log = Some(format!("could not write .nfo: {e}"));
                    None
                }
            }
        });
        let total_bytes = std::fs::metadata(&nzb_path).map(|m| m.len()).unwrap_or(0);

        let ctx = pesto::hooks::HookContext {
            name: release_name.clone(),
            total_bytes,
            server: cfg.host.clone(),
            group: cfg.groups.first().cloned().unwrap_or_default(),
            password: cfg
                .nzb_password
                .as_deref()
                .or(cfg.compress_password.as_deref())
                .unwrap_or("")
                .to_string(),
            nzb_path: nzb_path.to_string_lossy().into_owned(),
            nfo_path: nfo_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };

        let (ok, mut log) = pesto::hooks::run_one_hook(&hook, &ctx);
        if let Some(line) = nfo_log {
            log.insert(0, line);
        }
        let release_key = ui::components::file_tree::release_key(&release_name);
        let _ = tx.send(AppEvent::HooksDone {
            ok,
            release_key,
            release_name,
            hook_name,
            log,
        });
    });
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
fn trigger_prowlarr_queue_search(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
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

/// Called when the user presses 'u' on the Dashboard.
/// Delegates the full upload pipeline to `pesto::upload::run_upload`, which
/// handles compression, posting, NZB writing, history, indexer, and hooks.
fn handle_upload_trigger(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
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
            let mut season_nzb = None;
            if folder_mode == app::FolderMode::Season && !all_segments.is_empty() {
                if let Some(ref dir) = nzb_out_dir {
                    let out = dir.join(format!("{label}.nzb"));
                    let meta = pesto::nzb::NzbMeta {
                        name: Some(label.clone()),
                        password: config
                            .nzb_password
                            .clone()
                            .or_else(|| config.compress_password.clone()),
                        category: config.nzb_category.clone(),
                    };
                    let xml = pesto::nzb::generate(
                        &config.from,
                        &config.groups,
                        &all_segments,
                        &meta,
                        config.obfuscate == ObfuscateMode::Full,
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
                                run_season_hooks(&config, path, &label, &out, total_size, &tx)
                                    .await;
                            }

                            season_nzb = Some(out);
                        }
                        Err(e) => {
                            let _ =
                                tx.send(AppEvent::UploadError(format!("season nzb write: {e}")));
                            folder_ok = false;
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
        nzb_dir: None,
        indexer_url: None,
        indexer_api_key: None,
        history: false,
        history_dir: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        post_hook: None,
        no_hooks: true,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 0,
        check_retries: 1,
        pipeline_depth: 0,
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
/// `.nzb` when NFOs are enabled, build the same [`HookContext`], then run every
/// configured hook so the pack is forwarded to the indexer like the episodes.
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

    let ctx = pesto::hooks::HookContext {
        name: label.to_string(),
        total_bytes,
        server: config.host.clone(),
        group: config.groups.first().cloned().unwrap_or_default(),
        password: config
            .nzb_password
            .as_deref()
            .or(config.compress_password.as_deref())
            .unwrap_or("")
            .to_string(),
        nzb_path: nzb_path.to_string_lossy().into_owned(),
        nfo_path: nfo_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };

    let hook_cfg = config.clone();
    let log_lines = tokio::task::spawn_blocking(move || pesto::hooks::run_hooks(&hook_cfg, &ctx))
        .await
        .unwrap_or_else(|e| vec![format!("hook task panicked: {e}")]);
    for line in log_lines {
        let _ = tx.send(AppEvent::Progress(format!("[hook] {line}")));
    }
}

async fn run_real_upload(
    config: Config,
    entry_paths: Vec<PathBuf>,
    label: String,
    nzb_out_dir: Option<PathBuf>,
    tx: mpsc::UnboundedSender<AppEvent>,
    cancel_token: CancellationToken,
) -> anyhow::Result<pesto::upload::UploadOutcome> {
    // Bridge CancellationToken → AtomicBool (pesto's cancel mechanism).
    let cancel_flag = Arc::new(AtomicBool::new(false));
    {
        let flag = cancel_flag.clone();
        tokio::spawn(async move {
            cancel_token.cancelled().await;
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
        queue_extended: None,
        par2_hint_bytes: 0,
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
                        queue_extended: None,
            par2_hint_bytes: 0,
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

    Ok(outcome)
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
        E::Status { text } if !text.is_empty() => text.clone(),
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
                queue_extended: None,
                par2_hint_bytes: *par2_bytes_hint,
                par2_complete: false,
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
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_complete: false,
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
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_complete: false,
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
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_complete: false,
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
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_complete: false,
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
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_complete: false,
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
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_complete: false,
        }),
        E::Par2SliceWritten => {
            let (written, total) = match &previous.phase {
                Some(UploadPhase::WritingPar2 { written, total }) => (written + 1, *total),
                _ => (1, 1),
            };
            let all_written = total > 0 && written >= total;
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
                queue_extended: None,
                par2_hint_bytes: 0,
                par2_complete: all_written,
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
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_complete: false,
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
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_complete: false,
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
                queue_extended: None,
                par2_hint_bytes: 0,
                par2_complete: false,
            })
        }
        E::QueueExtended {
            segments, bytes, ..
        } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: None,
            par2_slices: None,
            queue_extended: Some((*segments, *bytes)),
            par2_hint_bytes: 0,
            par2_complete: false,
        }),
        _ => None,
    }
}
