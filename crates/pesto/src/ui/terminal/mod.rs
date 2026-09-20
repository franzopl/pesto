#[cfg(test)]
use crate::progress::RunMode;
use crate::progress::{ProgressEvent, ProgressReceiver, ProgressSender, RendererOptions};
#[cfg(test)]
use crate::ui::format::MIN_BODY_W;
#[cfg(test)]
use crate::ui::render::truncate;
use crate::ui::state::RenderState;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

mod panel;
mod plain;
mod quiet;
mod summary;

/// Spawn the built-in terminal renderer used by the `pesto` binary.
pub fn spawn_renderer() -> (ProgressSender, JoinHandle<()>) {
    spawn_renderer_with(RendererOptions::default())
}

/// Enable ANSI/VT100 escape processing on the stderr console handle.
///
/// Legacy Windows consoles (`conhost.exe`, old PowerShell/cmd hosts without
/// Windows Terminal) have `ENABLE_VIRTUAL_TERMINAL_PROCESSING` off by
/// default, so cursor-movement and SGR color sequences are printed as raw
/// text instead of being interpreted. Returns `false` when VT processing
/// could not be confirmed enabled, so callers can fall back to the
/// escape-free plain renderer instead of spamming garbled output.
#[cfg(windows)]
fn enable_ansi_support() -> bool {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_ERROR_HANDLE,
    };
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return false;
        }
        let mut mode: u32 = 0;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_ansi_support() -> bool {
    true
}

/// Like [`spawn_renderer`] but with explicit display options.
pub fn spawn_renderer_with(opts: RendererOptions) -> (ProgressSender, JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(render_loop(rx, opts));
    (tx, handle)
}

async fn render_loop(mut rx: ProgressReceiver, opts: RendererOptions) {
    // `opts.plain` forces the append-only branch even on a real terminal —
    // see its doc comment: the full/quiet panels both move the cursor, which
    // corrupts (and is corrupted by) verbose log lines sharing stderr.
    let tty = std::io::stderr().is_terminal() && enable_ansi_support() && !opts.plain;
    let mut state = RenderState::new();
    // Base interval; may be extended by adaptive logic when draws are slow.
    let mut interval_ms: u64 = 200;
    let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                None | Some(ProgressEvent::Finished) => {
                    state.finished = true;
                    if tty {
                        // Replace either live display with the same compact
                        // final summary. Quiet remains a single line while
                        // work is active, but completion must never truncate
                        // the outcome or an automatically recovered retry.
                        state.draw_summary();
                    } else {
                        state.draw_plain(true);
                    }
                    if opts.bell {
                        let mut err = std::io::stderr().lock();
                        let _ = err.write_all(b"\x07");
                        let _ = err.flush();
                    }
                    break;
                }
                Some(ev) => state.apply(ev),
            },
            _ = ticker.tick() => {
                if tty {
                    let draw_start = Instant::now();
                    if opts.quiet {
                        state.draw_quiet(false);
                    } else {
                        state.draw_panel(false);
                    }
                    // Adaptive refresh: back off to 500 ms when drawing is slow.
                    let draw_ms = draw_start.elapsed().as_millis() as u64;
                    let new_interval = if draw_ms > 5 { 500 } else { 200 };
                    if new_interval != interval_ms {
                        interval_ms = new_interval;
                        ticker = tokio::time::interval(Duration::from_millis(interval_ms));
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    }
                } else {
                    state.draw_plain(false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
