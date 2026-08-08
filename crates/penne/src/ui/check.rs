//! Live progress bar for `penne check` and `penne download --stat`.
//!
//! Simpler than [`crate::ui::terminal`]'s download panel: a `STAT`
//! check never fetches an article body, so the only numbers that matter
//! are how many segments have resolved so far, how fast they're resolving,
//! and when the check will finish. Still reuses [`pesto::ui::render`]'s
//! bar/width primitives so it looks like the same program as the download
//! panel, not a bolted-on afterthought.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use pesto::ui::render::{render_bar, terminal_width, truncate};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::check::{CheckProgress, CheckProgressReceiver};

const BAR_W: usize = 30;

/// Spawn the live check-progress bar. Redraws in place on a TTY; falls back
/// to a throttled plain-text log otherwise (matching
/// [`crate::ui::terminal::spawn_renderer`]'s split).
pub fn spawn_renderer(rx: CheckProgressReceiver, total_segments: u32) -> JoinHandle<()> {
    tokio::spawn(render_loop(rx, total_segments))
}

async fn render_loop(mut rx: CheckProgressReceiver, total_segments: u32) {
    let tty = std::io::stderr().is_terminal();
    let mut state = State::new(total_segments);
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                None => {
                    if tty {
                        state.draw();
                    } else {
                        state.draw_plain(true);
                    }
                    break;
                }
                Some(ev) => {
                    state.apply(ev);
                    // No periodic ticker drives the non-TTY path (see
                    // `ui::terminal`'s identical reasoning): a check against
                    // a fast/local server can finish inside one 200ms tick,
                    // so it must redraw here to ever observe an intermediate
                    // percentage. `draw_plain` dedupes repeats internally.
                    if !tty {
                        state.draw_plain(false);
                    }
                }
            },
            _ = ticker.tick() => {
                if tty {
                    state.draw();
                }
            }
        }
    }
}

struct State {
    total: u32,
    done: u32,
    missing: u32,
    /// When the check started, for rate/ETA calculation.
    start: Instant,
    /// Whether the single status line has been drawn once already, so a
    /// redraw knows to erase it first instead of appending a new one.
    drawn: bool,
    last_printed_pct: Option<u64>,
}

impl State {
    fn new(total: u32) -> Self {
        Self {
            total,
            done: 0,
            missing: 0,
            start: Instant::now(),
            drawn: false,
            last_printed_pct: None,
        }
    }

    fn apply(&mut self, ev: CheckProgress) {
        self.done += 1;
        if !ev.present {
            self.missing += 1;
        }
    }

    fn line(&self) -> String {
        let frac = if self.total > 0 {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let pct = (frac * 100.0).round() as u64;
        let bar = render_bar(frac, BAR_W);
        let mut line = format!(
            "checking  [{bar}] {pct:>3}%  {}/{} segments",
            self.done, self.total
        );
        if self.missing > 0 {
            line.push_str(&format!(" — {} missing", self.missing));
        }

        // Show articles/sec and ETA once enough time has elapsed for a
        // stable reading (avoids noisy ∞ or wildly fluctuating numbers
        // in the first few hundred milliseconds).
        let elapsed = self.start.elapsed();
        if elapsed.as_millis() > 500 && self.done > 0 {
            let secs = elapsed.as_secs_f64();
            let rate = self.done as f64 / secs;
            line.push_str(&format!("  {rate:.0} art/s"));

            let remaining = self.total.saturating_sub(self.done);
            if remaining > 0 && rate > 0.0 {
                let eta_secs = remaining as f64 / rate;
                let eta_m = eta_secs as u64 / 60;
                let eta_s = eta_secs as u64 % 60;
                line.push_str(&format!("  ETA {eta_m}:{eta_s:02}"));
            }
        }
        line
    }

    /// Redraw the single status line in place, same cursor-erase technique
    /// as `pesto::ui::terminal`'s quiet mode.
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
            "checking: {}/{} segments ({pct}%) — {} missing",
            self.done, self.total, self.missing
        );
        let _ = err.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reflects_progress_and_missing_count() {
        let mut state = State::new(4);
        assert!(state.line().contains("0/4 segments"));
        assert!(!state.line().contains("missing"));

        state.apply(CheckProgress { present: true });
        state.apply(CheckProgress { present: false });
        let line = state.line();
        assert!(line.contains("2/4 segments"));
        assert!(line.contains("50%"));
        assert!(line.contains("1 missing"));
    }

    #[test]
    fn empty_queue_reports_100_percent_without_dividing_by_zero() {
        let state = State::new(0);
        assert!(state.line().contains("100%"));
    }

    #[test]
    fn rate_and_eta_shown_after_startup_period() {
        let mut state = State::new(100);
        // Backdate the start time to simulate elapsed time.
        state.start = Instant::now() - Duration::from_secs(2);
        for _ in 0..50 {
            state.apply(CheckProgress { present: true });
        }
        let line = state.line();
        assert!(line.contains("art/s"), "expected rate in: {line}");
        assert!(line.contains("ETA"), "expected ETA in: {line}");
    }
}
