//! Live terminal renderer for `penne download`'s progress panel.
//!
//! Mirrors `pesto::ui::terminal`'s posting panel — same box-drawn frame, same
//! sub-character-smooth bars, same "redraw in place on a TTY, throttled
//! one-line-per-change log otherwise" split — reusing its generic rendering
//! primitives from [`pesto::ui::render`] rather than re-implementing them
//! (see `CLAUDE.md`: "shared types... live in `pesto`... `penne` reuses
//! them").
//!
//! The unit here is the *file*, not the connection: `penne`'s worker pool
//! pulls segments from a queue that spans every file at once, so "which
//! connection is doing what" isn't a meaningful thing to show a user
//! downloading a release — "which files are in flight" is. A release can
//! easily ship 50+ RAR/PAR2 volumes, so only the busiest [`FILE_LIMIT`]
//! files ever get their own bar; the rest collapse into a single summary
//! line, the same way `pesto`'s connection grid collapses past
//! `GRID_LIMIT`.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use pesto::progress::format_size;
use pesto::ui::render::{
    ansi, box_bottom, box_line, box_top, format_duration, pad, render_bar, render_sparkline,
    terminal_width, truncate,
};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::progress::{ProgressEvent, ProgressReceiver};

/// Width, in characters, of the overall-progress box interior.
const BODY_W: usize = 56;
/// Above this many in-flight files, the rest collapse into one summary line
/// instead of getting their own bar — see the module docs.
const FILE_LIMIT: usize = 8;
/// Width of each per-file bar.
const FILE_BAR_W: usize = 20;
/// Width of the (truncated/padded) file name column before each bar.
const NAME_COL_W: usize = 22;

/// Spawn the live progress renderer. Draws a full panel on a TTY (redrawn in
/// place); falls back to a throttled plain-text log when stderr isn't a
/// terminal (e.g. output redirected to a file or captured by a test
/// harness).
pub fn spawn_renderer(rx: ProgressReceiver) -> JoinHandle<()> {
    tokio::spawn(render_loop(rx))
}

async fn render_loop(mut rx: ProgressReceiver) {
    let tty = std::io::stderr().is_terminal();
    let mut state = RenderState::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                None => {
                    if tty {
                        state.draw_panel(true);
                    } else {
                        state.draw_plain(true);
                    }
                    break;
                }
                Some(ev) => {
                    state.apply(ev);
                    // Non-TTY output has no periodic ticker (below) driving
                    // it, so it must redraw here — on a fast local transfer
                    // (e.g. a test against a loopback fake server) a whole
                    // download can complete well inside one 200ms tick, and
                    // an intermediate percentage would otherwise never be
                    // observed. `draw_plain` internally dedupes repeats by
                    // percentage, so this is cheap even under heavy event
                    // traffic.
                    if !tty {
                        state.draw_plain(false);
                    }
                }
            },
            _ = ticker.tick() => {
                if tty {
                    state.draw_panel(false);
                }
            }
        }
    }
}

/// Per-file progress, seeded from [`ProgressEvent::Started`] and updated as
/// segments for that file resolve.
struct FileState {
    name: String,
    total_segments: u32,
    done_segments: u32,
    assembled: bool,
}

impl FileState {
    /// A file with every segment resolved (fetched, missing, or corrupt —
    /// all three are "done" in the sense that no more network work is
    /// coming for it) or already assembled.
    fn is_done(&self) -> bool {
        self.assembled || (self.total_segments > 0 && self.done_segments >= self.total_segments)
    }
}

/// Mutable view the renderer builds up from the event stream.
struct RenderState {
    started: bool,
    start: Instant,
    total_bytes: u64,
    done_bytes: u64,
    total_segments: u32,
    done_segments: u32,
    missing: u32,
    corrupt: u32,
    /// Kept in queue order (as announced by `Started`) so which files get a
    /// bar stays stable across redraws instead of jittering with a
    /// `HashMap`'s arbitrary iteration order.
    files: Vec<FileState>,
    /// File names assembled since the last flush, queued so a TTY redraw
    /// can erase the panel, print them as permanent scrollback, then redraw
    /// fresh below — mirroring how the previous flat-line renderer cleared
    /// its line before announcing a finished file.
    newly_assembled: Vec<String>,
    /// Lines emitted by the previous panel draw, to be cleared on the next.
    lines_drawn: usize,
    /// Rolling window of bytes-per-second samples (up to 10 entries, ~2s at
    /// the 200ms tick rate), for the sparkline and ETA.
    speed_history: [f64; 10],
    speed_history_pos: usize,
    speed_history_len: usize,
    /// Bytes done at the last tick, for computing the per-tick delta.
    prev_done_bytes: u64,
    /// Last percentage printed in plain (non-TTY) mode, so repeats collapse
    /// instead of spamming the log on every single segment.
    last_printed_pct: Option<u64>,
}

impl RenderState {
    fn new() -> Self {
        Self {
            started: false,
            start: Instant::now(),
            total_bytes: 0,
            done_bytes: 0,
            total_segments: 0,
            done_segments: 0,
            missing: 0,
            corrupt: 0,
            files: Vec::new(),
            newly_assembled: Vec::new(),
            lines_drawn: 0,
            speed_history: [0.0; 10],
            speed_history_pos: 0,
            speed_history_len: 0,
            prev_done_bytes: 0,
            last_printed_pct: None,
        }
    }

    fn apply(&mut self, ev: ProgressEvent) {
        match ev {
            ProgressEvent::Started { files } => {
                self.started = true;
                self.start = Instant::now();
                for f in files {
                    self.total_segments += f.segments;
                    self.total_bytes += f.bytes;
                    self.files.push(FileState {
                        name: f.name,
                        total_segments: f.segments,
                        done_segments: 0,
                        assembled: false,
                    });
                }
            }
            ProgressEvent::SegmentDownloaded {
                file_name, bytes, ..
            } => {
                self.done_segments += 1;
                self.done_bytes += bytes;
                if let Some(f) = self.files.iter_mut().find(|f| f.name == file_name) {
                    f.done_segments += 1;
                }
            }
            ProgressEvent::SegmentMissing { file_name, .. } => {
                self.done_segments += 1;
                self.missing += 1;
                if let Some(f) = self.files.iter_mut().find(|f| f.name == file_name) {
                    f.done_segments += 1;
                }
            }
            ProgressEvent::SegmentCorrupt { file_name, .. } => {
                self.done_segments += 1;
                self.corrupt += 1;
                if let Some(f) = self.files.iter_mut().find(|f| f.name == file_name) {
                    f.done_segments += 1;
                }
            }
            ProgressEvent::FileAssembled { file_name } => {
                if let Some(f) = self.files.iter_mut().find(|f| f.name == file_name) {
                    f.assembled = true;
                }
                self.newly_assembled.push(file_name);
            }
        }
    }

    fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64().max(0.001)
    }

    /// Bytes fetched per second so far, averaged over the whole run.
    fn rate(&self) -> f64 {
        self.done_bytes as f64 / self.elapsed_secs()
    }

    fn push_speed_sample(&mut self, bps: f64) {
        self.speed_history[self.speed_history_pos] = bps;
        self.speed_history_pos = (self.speed_history_pos + 1) % 10;
        if self.speed_history_len < 10 {
            self.speed_history_len += 1;
        }
    }

    fn speed_samples(&self) -> Vec<f64> {
        let n = self.speed_history_len;
        if n == 0 {
            return Vec::new();
        }
        let start = if n < 10 { 0 } else { self.speed_history_pos };
        (0..n)
            .map(|i| self.speed_history[(start + i) % 10])
            .collect()
    }

    /// A single-point ETA estimate from the mean of recent speed samples.
    /// Unlike `pesto`'s posting panel, this doesn't track a confidence
    /// range — a plain estimate is enough to answer "roughly how long is
    /// left", and a full variance-based range isn't worth the extra state
    /// for one number.
    fn eta_secs(&self) -> Option<f64> {
        let remaining = self.total_bytes.saturating_sub(self.done_bytes) as f64;
        if remaining <= 0.0 {
            return None;
        }
        let samples = self.speed_samples();
        if samples.is_empty() {
            return None;
        }
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        if mean < 1.0 {
            return None;
        }
        Some(remaining / mean)
    }

    /// Files fully resolved, actively downloading (at least one segment
    /// done but not all), and not yet started.
    fn file_tally(&self) -> (usize, usize, usize) {
        let mut done = 0;
        let mut active = 0;
        let mut waiting = 0;
        for f in &self.files {
            if f.is_done() {
                done += 1;
            } else if f.done_segments > 0 {
                active += 1;
            } else {
                waiting += 1;
            }
        }
        (done, active, waiting)
    }

    // ---- TTY panel rendering --------------------------------------------

    /// Erase the previously drawn panel (cursor up `lines_drawn`, clear to
    /// end of screen) so the next thing written starts from a clean slate —
    /// used both before redrawing the panel itself and before printing a
    /// permanent "assembled" scrollback line above it.
    fn erase_panel(&mut self) {
        if self.lines_drawn == 0 {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\x1b[{}A\r\x1b[0J", self.lines_drawn);
        let _ = err.flush();
        self.lines_drawn = 0;
    }

    /// Print any files assembled since the last flush as permanent
    /// scrollback lines above the panel, erasing the panel first so they
    /// don't get overwritten by the next redraw.
    fn flush_assembled_panel(&mut self) {
        if self.newly_assembled.is_empty() {
            return;
        }
        self.erase_panel();
        let mut err = std::io::stderr().lock();
        for name in self.newly_assembled.drain(..) {
            let _ = writeln!(err, "  assembled: {name}");
        }
        let _ = err.flush();
    }

    fn draw_panel(&mut self, final_draw: bool) {
        if !self.started {
            return;
        }
        self.flush_assembled_panel();

        let current_bps =
            self.done_bytes.saturating_sub(self.prev_done_bytes) as f64 * (1000.0 / 200.0);
        self.prev_done_bytes = self.done_bytes;
        if !final_draw && self.done_bytes > 0 {
            self.push_speed_sample(current_bps);
        }

        // Every line must fit one physical terminal row, or the
        // cursor-up-by-logical-line-count redraw below desyncs — see
        // `pesto::ui::terminal::draw_panel`'s identical comment.
        let width = terminal_width().unwrap_or(80).max(20);
        let lines: Vec<String> = self
            .panel_lines(final_draw)
            .into_iter()
            .map(|l| truncate(&l, width))
            .collect();

        let mut out = String::new();
        if self.lines_drawn > 0 {
            out.push_str(&format!("\x1b[{}A\r", self.lines_drawn));
        }
        out.push_str("\x1b[0J");
        for line in &lines {
            out.push_str(line);
            out.push('\n');
        }
        self.lines_drawn = lines.len();

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }

    fn panel_lines(&self, final_draw: bool) -> Vec<String> {
        let mut lines = Vec::new();

        lines.push(format!(
            "penne  downloading  {} file(s) · {}",
            self.files.len(),
            format_duration(self.elapsed_secs())
        ));

        let frac = if self.total_segments > 0 {
            (self.done_segments as f64 / self.total_segments as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let pct = (frac * 100.0).round() as u64;
        let bar = render_bar(frac, 26);
        let line1 = format!(
            "[{bar}] {pct:>3}%  {}/{} seg",
            self.done_segments, self.total_segments
        );
        let rate = self.rate();
        let (line2, line3) = if final_draw {
            let avg_speed = if self.elapsed_secs() > 0.001 {
                self.done_bytes as f64 / self.elapsed_secs()
            } else {
                0.0
            };
            let summary = format!(
                "{} · avg {}/s · elapsed {}",
                format_size(self.done_bytes),
                format_size(avg_speed as u64),
                format_duration(self.elapsed_secs()),
            );
            (summary, None)
        } else {
            let spark = {
                let samples = self.speed_samples();
                let wide_enough = terminal_width().is_none_or(|w| w >= 60);
                if samples.len() >= 2 && wide_enough {
                    format!(" {}", render_sparkline(&samples))
                } else {
                    String::new()
                }
            };
            let l2 = format!(
                "{}/{} · {}/s{}",
                format_size(self.done_bytes),
                format_size(self.total_bytes),
                format_size(rate as u64),
                spark,
            );
            let l3 = match self.eta_secs() {
                Some(secs) => format!("ETA {}", format_duration(secs)),
                None => "ETA —".to_string(),
            };
            (l2, Some(l3))
        };
        lines.push(box_top("download", BODY_W));
        lines.push(box_line(&line1, BODY_W));
        lines.push(box_line(&line2, BODY_W));
        if let Some(l3) = line3 {
            lines.push(box_line(&l3, BODY_W));
        }
        lines.push(box_bottom(BODY_W));

        // --- per-file bars, capped at FILE_LIMIT --------------------------
        let mut active: Vec<&FileState> = Vec::new();
        let mut waiting: Vec<&FileState> = Vec::new();
        for f in &self.files {
            if f.is_done() {
                continue;
            }
            if f.done_segments > 0 {
                active.push(f);
            } else {
                waiting.push(f);
            }
        }
        let in_flight = active.len() + waiting.len();
        let shown = active.iter().chain(waiting.iter()).take(FILE_LIMIT);
        let mut shown_count = 0;
        for f in shown {
            lines.push(file_bar_line(f));
            shown_count += 1;
        }
        let remaining = in_flight - shown_count;
        if remaining > 0 {
            lines.push(format!("+{remaining} more waiting"));
        }

        // --- file tally + failures ----------------------------------------
        let (done, active_n, waiting_n) = self.file_tally();
        let mut tally = format!(
            "files  done {done}/{}  downloading {active_n}  waiting {waiting_n}",
            self.files.len()
        );
        if self.missing > 0 {
            tally.push_str(&ansi(&format!("  {} missing", self.missing), "31"));
        }
        if self.corrupt > 0 {
            tally.push_str(&ansi(&format!("  {} corrupt", self.corrupt), "33"));
        }
        lines.push(tally);

        lines
    }

    // ---- non-TTY plain rendering ----------------------------------------

    fn draw_plain(&mut self, final_draw: bool) {
        if !self.started {
            return;
        }
        if !self.newly_assembled.is_empty() {
            let mut err = std::io::stderr().lock();
            for name in self.newly_assembled.drain(..) {
                let _ = writeln!(err, "  assembled: {name}");
            }
            let _ = err.flush();
        }

        let pct = if self.total_segments > 0 {
            (self.done_segments as u64 * 100) / self.total_segments as u64
        } else {
            100
        };
        if !final_draw && self.last_printed_pct == Some(pct) {
            return;
        }
        self.last_printed_pct = Some(pct);

        let rate = self.rate();
        let mut err = std::io::stderr().lock();
        if final_draw {
            let _ = writeln!(
                err,
                "done: {}/{} segments ({pct}%) · {} · avg {}/s · {} missing, {} corrupt · {}",
                self.done_segments,
                self.total_segments,
                format_size(self.done_bytes),
                format_size(rate as u64),
                self.missing,
                self.corrupt,
                format_duration(self.elapsed_secs()),
            );
        } else {
            let _ = writeln!(
                err,
                "fetching: {}/{} segments ({pct}%) · {} · {}/s — {} missing, {} corrupt",
                self.done_segments,
                self.total_segments,
                format_size(self.done_bytes),
                format_size(rate as u64),
                self.missing,
                self.corrupt,
            );
        }
        let _ = err.flush();
    }
}

/// One `{name} [{bar}] {pct}% {done}/{total} seg` line for a single
/// in-flight file.
fn file_bar_line(f: &FileState) -> String {
    let frac = if f.total_segments > 0 {
        (f.done_segments as f64 / f.total_segments as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let pct = (frac * 100.0).round() as u64;
    let bar = render_bar(frac, FILE_BAR_W);
    let name = pad(&truncate(&f.name, NAME_COL_W), NAME_COL_W);
    format!(
        "{name} [{bar}] {pct:>3}%  {}/{} seg",
        f.done_segments, f.total_segments
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::FileEntry;

    fn started(names: &[&str], segments_each: u32, bytes_each: u64) -> ProgressEvent {
        ProgressEvent::Started {
            files: names
                .iter()
                .map(|n| FileEntry {
                    name: n.to_string(),
                    segments: segments_each,
                    bytes: bytes_each,
                })
                .collect(),
        }
    }

    #[test]
    fn started_seeds_totals_and_preserves_queue_order() {
        let mut state = RenderState::new();
        state.apply(started(&["a.bin", "b.bin", "c.bin"], 10, 1000));
        assert_eq!(state.total_segments, 30);
        assert_eq!(state.total_bytes, 3000);
        assert_eq!(
            state
                .files
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a.bin", "b.bin", "c.bin"]
        );
    }

    #[test]
    fn segment_events_update_both_global_and_per_file_state() {
        let mut state = RenderState::new();
        state.apply(started(&["a.bin", "b.bin"], 2, 200));
        state.apply(ProgressEvent::SegmentDownloaded {
            file_name: "a.bin".into(),
            part: 1,
            bytes: 100,
        });
        state.apply(ProgressEvent::SegmentMissing {
            file_name: "b.bin".into(),
            part: 1,
        });

        assert_eq!(state.done_segments, 2);
        assert_eq!(state.done_bytes, 100);
        assert_eq!(state.missing, 1);
        assert_eq!(state.files[0].done_segments, 1);
        assert_eq!(state.files[1].done_segments, 1);
        assert!(!state.files[1].is_done()); // 1/2, still in flight
    }

    #[test]
    fn more_than_file_limit_in_flight_files_collapse_to_one_summary_line() {
        let mut state = RenderState::new();
        let names: Vec<String> = (0..12).map(|i| format!("file{i}.bin")).collect();
        let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
        state.apply(started(&name_refs, 10, 1000));
        // Give every file at least one downloaded segment so all 12 count
        // as "active" and would, without the cap, each get their own line.
        for name in &names {
            state.apply(ProgressEvent::SegmentDownloaded {
                file_name: name.clone(),
                part: 1,
                bytes: 100,
            });
        }

        let lines = state.panel_lines(false);
        let bar_lines = lines
            .iter()
            .filter(|l| l.trim_start().starts_with("file") && l.contains("] "))
            .count();
        // FILE_LIMIT bars, not one per file.
        assert_eq!(bar_lines, FILE_LIMIT);
        assert!(
            lines.iter().any(|l| l.contains("more waiting")),
            "expected a collapse line for files beyond the cap: {lines:?}"
        );
    }

    #[test]
    fn done_files_are_not_shown_as_bars() {
        let mut state = RenderState::new();
        state.apply(started(&["a.bin"], 1, 100));
        state.apply(ProgressEvent::SegmentDownloaded {
            file_name: "a.bin".into(),
            part: 1,
            bytes: 100,
        });
        assert!(state.files[0].is_done());

        let lines = state.panel_lines(false);
        assert!(
            !lines.iter().any(|l| l.starts_with("a.bin")),
            "a fully-downloaded file shouldn't still show its own bar: {lines:?}"
        );
    }

    #[test]
    fn file_assembled_marks_file_done_and_queues_announcement() {
        let mut state = RenderState::new();
        state.apply(started(&["a.bin"], 1, 100));
        state.apply(ProgressEvent::FileAssembled {
            file_name: "a.bin".into(),
        });
        assert!(state.files[0].assembled);
        assert_eq!(state.newly_assembled, vec!["a.bin".to_string()]);
    }
}
