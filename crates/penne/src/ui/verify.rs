//! Live progress bar for the full PAR2 verify pass in `penne download`.
//!
//! Only ever draws anything when [`crate::repair::verify_and_repair`]
//! actually falls back to a real, byte-exact re-hash — the common case
//! (a release whose downloaded bytes already match what PAR2 expects) is
//! caught by [`crate::quickcheck`] first and never sends a single
//! [`crate::repair::VerifyProgress`] event, so this renderer sits quiet
//! and [`spawn_renderer`]'s caller sees no bar at all.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use pesto::ui::render::{render_bar, terminal_width, truncate};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::repair::{VerifyProgress, VerifyProgressReceiver};

const BAR_W: usize = 30;

/// Spawn the live verify-progress bar. Redraws in place on a TTY; falls
/// back to a throttled plain-text log otherwise (matching
/// [`crate::ui::check::spawn_renderer`]'s split). Returns `true` once the
/// channel closes if at least one progress event was ever received — the
/// caller's signal that a real verify pass ran, as opposed to being
/// skipped by the quick-check.
pub fn spawn_renderer(rx: VerifyProgressReceiver) -> JoinHandle<bool> {
    tokio::spawn(render_loop(rx))
}

async fn render_loop(mut rx: VerifyProgressReceiver) -> bool {
    let tty = std::io::stderr().is_terminal();
    let mut state: Option<State> = None;
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                None => {
                    if let Some(state) = &mut state {
                        if tty {
                            state.draw();
                        } else {
                            state.draw_plain(true);
                        }
                    }
                    break;
                }
                Some(ev) => {
                    let state = state.get_or_insert_with(State::new);
                    state.apply(ev);
                    if !tty {
                        state.draw_plain(false);
                    }
                }
            },
            _ = ticker.tick() => {
                if let Some(state) = &mut state {
                    if tty {
                        state.draw();
                    }
                }
            }
        }
    }

    state.is_some()
}

struct State {
    total: usize,
    done: usize,
    file_name: String,
    /// Whether the single status line has been drawn once already, so a
    /// redraw knows to erase it first instead of appending a new one.
    drawn: bool,
    last_printed_pct: Option<u64>,
}

impl State {
    fn new() -> Self {
        Self {
            total: 0,
            done: 0,
            file_name: String::new(),
            drawn: false,
            last_printed_pct: None,
        }
    }

    fn apply(&mut self, ev: VerifyProgress) {
        self.done = ev.slices_done;
        self.total = ev.total_slices;
        self.file_name = ev.file_name;
    }

    fn line(&self) -> String {
        let frac = if self.total > 0 {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let pct = (frac * 100.0).round() as u64;
        let bar = render_bar(frac, BAR_W);
        format!("verifying [{bar}] {pct:>3}%  {}", self.file_name)
    }

    /// Redraw the single status line in place, same cursor-erase technique
    /// as [`crate::ui::check::State::draw`].
    fn draw(&mut self) {
        let width = terminal_width().unwrap_or(80).max(20);
        let line = truncate(&self.line(), width);

        let mut out = String::new();
        if self.drawn {
            out.push_str("\x1b[1A\r\x1b[2K");
        }
        out.push_str(&line);
        out.push('\n');
        self.drawn = true;

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }

    fn draw_plain(&mut self, final_draw: bool) {
        let pct = if self.total > 0 {
            (self.done as u64 * 100) / self.total as u64
        } else {
            100
        };
        if !final_draw && self.last_printed_pct == Some(pct) {
            return;
        }
        self.last_printed_pct = Some(pct);

        let mut err = std::io::stderr().lock();
        let _ = writeln!(
            err,
            "verifying: {}/{} slices ({pct}%) — {}",
            self.done, self.total, self.file_name
        );
        let _ = err.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reflects_progress_and_current_file() {
        let mut state = State::new();
        state.apply(VerifyProgress {
            file_name: "movie.mkv".to_string(),
            slices_done: 5,
            total_slices: 20,
        });
        let line = state.line();
        assert!(line.contains("25%"));
        assert!(line.contains("movie.mkv"));
    }

    #[test]
    fn no_progress_reports_full_bar_without_dividing_by_zero() {
        let state = State::new();
        assert!(state.line().contains("100%"));
    }
}
