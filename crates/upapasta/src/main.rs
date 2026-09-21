use std::{io, time::Duration};

use crossterm::{
    event::{Event, EventStream, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

mod app;
mod catalog;
mod events;
mod input;
mod nzb_viewer;
mod prowlarr;
mod runtime;
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

    let res = runtime::run_app(
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
