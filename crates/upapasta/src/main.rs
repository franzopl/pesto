use std::{io, path::PathBuf, time::Duration};

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
mod events;
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

    // Spawn a background task that simulates pesto progress events
    // (this will be replaced by real pesto::post() receiver in the next step)
    let tx_progress = tx.clone();
    tokio::spawn(async move {
        let mut i = 0u32;
        loop {
            tokio::time::sleep(Duration::from_millis(380)).await;
            let _ = tx_progress.send(AppEvent::Progress(format!(
                "article {}/{} posted @ {:.1} MB/s",
                i % 47 + 1,
                47,
                12.4 + (i as f32 % 5.0) * 0.3
            )));
            i += 1;
            if i.is_multiple_of(11) {
                let _ = tx_progress.send(AppEvent::Progress("PAR2 block verified".into()));
            }
        }
    });

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
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Tab => app.next_tab(),
                    KeyCode::BackTab => app.prev_tab(),
                    KeyCode::Char('h') if app.state == app::AppState::Browser => {
                        app.file_tree.toggle_hidden();
                    }
                    _ if app.state == app::AppState::Browser => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => app.file_tree.select_previous(),
                        KeyCode::Down | KeyCode::Char('j') => app.file_tree.select_next(),
                        KeyCode::Enter => {
                            if let Some(selected) = app.file_tree.get_selected().cloned() {
                                if selected.is_dir() {
                                    app.file_tree.current_dir = selected;
                                    app.file_tree.refresh();
                                    app.file_tree.selected = 0;
                                } else {
                                    let path_str = selected.to_string_lossy().to_string();
                                    // Toggle: if already in queue, remove it
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
                            handle_upload_trigger(app, tx.clone());
                        }
                        _ => {}
                    },
                    KeyCode::Char('u') if app.state == app::AppState::Dashboard => {
                        handle_upload_trigger(app, tx.clone());
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
                    KeyCode::Char('a') if app.state == app::AppState::Dashboard => {
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
                    // Navigate queue with Shift or dedicated keys when on Dashboard
                    KeyCode::Char('J') if app.state == app::AppState::Dashboard => {
                        app.upload_queue.select_next();
                    }
                    KeyCode::Char('K') if app.state == app::AppState::Dashboard => {
                        app.upload_queue.select_previous();
                    }
                    _ => {}
                },
                AppEvent::Progress(msg) => {
                    app.handle_progress(msg);
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

/// Called when the user presses 'u' on the Dashboard.
/// Starts a real (dry-run) upload using pesto::post() and streams progress.
fn handle_upload_trigger(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    app.trigger_upload();

    // Snapshot the files currently in the queue
    let files: Vec<PathBuf> = app.upload_queue.items.iter().map(PathBuf::from).collect();

    if files.is_empty() {
        return;
    }

    // Use real config if loaded, otherwise fall back to dry-run
    let config = if let Some(cfg) = &app.pesto_config {
        // Clone because we may want to force dry_run in future UI options
        let mut real_cfg = cfg.clone();
        // For now, always do real upload when config exists
        real_cfg.dry_run = false;
        real_cfg
    } else {
        build_dry_run_config()
    };

    // Convert to pesto InputFile
    let input_files: Vec<InputFile> = files
        .into_iter()
        .map(|p| InputFile {
            path: p.clone(),
            name: p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned()),
        })
        .collect();

    // Token is already created and stored in app.current_cancel_token by trigger_upload
    let cancel_token = app.current_cancel_token.clone().unwrap_or_default();

    // Spawn the actual upload task
    tokio::spawn(async move {
        let tx2 = tx.clone();
        let result = run_real_upload(config, input_files, tx.clone(), cancel_token).await;

        let success = result.is_ok();
        let cancelled = result.as_ref().is_ok_and(|r| r.cancelled);
        if let Err(e) = result {
            if !e.is_cancelled() {
                let _ = tx2.send(AppEvent::Progress(format!("ERROR: {}", e)));
            }
        }
        let _ = tx2.send(AppEvent::UploadFinished { success, cancelled });
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

/// The actual async upload worker. Forwards every ProgressEvent into the UI.
/// Supports cancellation via CancellationToken.
async fn run_real_upload(
    config: Config,
    files: Vec<InputFile>,
    tx: mpsc::UnboundedSender<AppEvent>,
    cancel_token: CancellationToken,
) -> Result<UploadResult, UploadError> {
    let (outcome, mut rx) = match pesto::post(config, files).await {
        Ok(v) => v,
        Err(e) => return Err(UploadError::Pesto(e.to_string())),
    };

    let mut last_update = ProgressUpdate {
        done_segments: 0,
        total_segments: 0,
        done_bytes: 0,
        total_bytes: 0,
        current_speed_mbps: 0.0,
        message: None,
        file_update: None,
    };

    loop {
        tokio::select! {
            biased;

            _ = cancel_token.cancelled() => {
                // Best effort: drop the receiver to let pesto wind down
                drop(rx);
                return Ok(UploadResult { cancelled: true });
            }

            ev = rx.recv() => {
                match ev {
                    Some(event) => {
                        // Send human readable log
                        let msg = format_progress_event(&event);
                        if !msg.is_empty() {
                            let _ = tx.send(AppEvent::Progress(msg));
                        }

                        // Build structured update when possible
                        if let Some(update) = extract_progress_update(&event, &last_update) {
                            last_update = update.clone();
                            let _ = tx.send(AppEvent::ProgressUpdate(update));
                        }

                        if matches!(event, pesto::progress::ProgressEvent::Finished) {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }

    // Final outcome summary
    let _ = tx.send(AppEvent::Progress(format!(
        "PostOutcome: {} segments, {} failures",
        outcome.segments.len(),
        outcome.failures.len()
    )));

    Ok(UploadResult { cancelled: false })
}

#[derive(Debug)]
struct UploadResult {
    cancelled: bool,
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
        E::SegmentDone { file, bytes, ok } => {
            format!(
                "Segment {} — {} ({})",
                if *ok { "ok" } else { "FAIL" },
                file,
                bytes
            )
        }
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
        E::Par2InputProgress { done, total } => {
            format!("PAR2 input pass: {}/{}", done, total)
        }
        E::Par2SliceWritten => "PAR2 slice written".into(),
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
    use pesto::progress::ProgressEvent as E;

    match ev {
        E::Started { files, .. } => {
            let total_segments: u64 = files.iter().map(|f| f.segments).sum();
            let total_bytes: u64 = files.iter().map(|f| f.bytes).sum();
            Some(ProgressUpdate {
                done_segments: 0,
                total_segments,
                done_bytes: 0,
                total_bytes,
                current_speed_mbps: 0.0,
                message: None,
                file_update: None,
            })
        }
        E::SegmentDone { file, bytes, ok } => {
            let file_update = FileProgressUpdate {
                name: file.clone(),
                done_segments: 1, // incremental
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
                message: Some(format!(
                    "Segment {} — {}",
                    if *ok { "ok" } else { "FAIL" },
                    file
                )),
                file_update: Some(file_update),
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
        }),
        E::Par2InputProgress { done, total } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: Some(format!("PAR2 input: {}/{}", done, total)),
            file_update: None,
        }),
        _ => None,
    }
}
