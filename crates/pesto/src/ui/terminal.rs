use crate::progress::{ProgressEvent, ProgressReceiver, ProgressSender, RendererOptions, RunMode};
use crate::ui::render::{
    ansi, box_bottom, box_line, box_top, format_duration, render_bar, render_sparkline,
    terminal_width, truncate, SUBCHAR,
};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Bounds on the panel box interior width. The width itself tracks the
/// terminal (see [`body_width`]) instead of being fixed: a 50-column window
/// used to have every box border truncated away by the `truncate` call in
/// [`RenderState::draw_panel`], and a 120-column one left half the screen
/// empty.
const MIN_BODY_W: usize = 24;
const MAX_BODY_W: usize = 100;
/// Above this connection count the per-connection grid is replaced by a
/// one-line summary, so the panel never grows unbounded.
const GRID_LIMIT: usize = 12;

/// Interior width of the panel boxes on a terminal `width` columns wide.
///
/// [`box_top`], [`box_line`] and [`box_bottom`] all render `body + 4` visible
/// columns (`│ ` … ` │`), so this is just the terminal minus that frame.
fn body_width(width: usize) -> usize {
    width.saturating_sub(4).clamp(MIN_BODY_W, MAX_BODY_W)
}

/// Bar width for a given box interior, proportional so the figures to the
/// bar's right keep their room on a narrow terminal. The ratio reproduces the
/// 26-in-56 bar the fixed-width panel used.
fn bar_width(body_w: usize) -> usize {
    (body_w * 46 / 100).clamp(10, 40)
}

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
                        if opts.quiet {
                            state.draw_quiet(true);
                        } else {
                            // Replace the live panel with a compact two-line
                            // summary rather than leaving the full frame —
                            // idle dot row, empty par2 lines and all — frozen
                            // in the scrollback.
                            state.draw_summary();
                        }
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

/// Visual state of a single NNTP connection worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ConnState {
    #[default]
    Idle,
    Busy,
    Auth,
    Retrying,
}

/// Mutable view the renderer builds up from the event stream.
#[allow(dead_code)]
struct RenderState {
    started: bool,
    finished: bool,
    mode: RunMode,
    target: Option<String>,
    start: Instant,
    total_segments: u64,
    total_bytes: u64,
    done_segments: u64,
    done_bytes: u64,
    failures: u64,
    interrupted: bool,
    status: String,
    /// When the current non-empty status text was first set.
    status_since: Option<Instant>,
    /// Last `status` text printed by `draw_plain` — lets it print each new
    /// status line exactly once instead of never (every phase branch
    /// `return`s before reaching a generic status print) or every tick.
    plain_status_printed: String,
    /// Whether the one-time "connections: N upload · M check" line has
    /// already been printed in plain (non-TTY) mode.
    plain_connections_printed: bool,
    /// Number of NNTP connections dedicated to the streaming STAT check,
    /// separate from `conn_files`/`conn_state` (upload connections only).
    /// 0 when checking is disabled.
    check_connections: usize,
    /// File currently posted by each worker connection (`None` = idle).
    conn_files: Vec<Option<String>>,
    /// Per-file `(done, total)` segment counts, for the file tally.
    files: HashMap<String, (u64, u64)>,
    /// Lines emitted by the previous panel draw, to be cleared on the next.
    lines_drawn: usize,
    /// Tick counter that paces the non-TTY plain output.
    plain_ticks: u32,
    /// Rolling window of bytes-per-second samples (up to 10 entries).
    speed_history: [f64; 10],
    speed_history_pos: usize,
    speed_history_len: usize,
    /// Bytes done at the last tick, for computing per-tick delta.
    prev_done_bytes: u64,
    /// Spinner frame index for quiet mode.
    spinner_frame: usize,
    /// Connection state overrides: None=normal, Some(ConnState).
    conn_state: Vec<ConnState>,
    /// Buffer pool snapshot.
    buf_total: usize,
    buf_free: usize,
    /// PAR2 bytes hint included in total_bytes upfront; reduced as QueueExtended arrives.
    par2_hint_remaining: u64,
    /// PAR2 segments hint included in total_segments upfront; reduced as
    /// QueueExtended arrives, mirroring `par2_hint_remaining` for bytes.
    par2_segment_hint_remaining: u64,
    /// Whether any QueueExtended event was received (PAR2 files being posted).
    posting_par2: bool,
    /// Whether `-v` is active. Diagnostics that are noise during a healthy
    /// run (the process RSS/CPU line) are gated on it, and the `/proc/self`
    /// polling that feeds them is skipped entirely otherwise.
    verbose: bool,
    // Process resource stats (polled from /proc/self on Linux)
    proc_rss_bytes: u64,
    proc_cpu_pct: f64,
    /// Previous (utime+stime) ticks for CPU delta.
    proc_prev_ticks: u64,
    proc_prev_tick_time: Instant,
    // Compression phase
    compress_active: bool,
    compress_total: u64,
    compress_written: u64,
    compress_start: Instant,
    // PAR2 recovery slice writing phase
    par2_write_active: bool,
    par2_write_total: u32,
    par2_write_done: u32,
    par2_write_start: Instant,
    /// Recovery-slice count announced upfront by `Par2EncodeStarted`, so the
    /// combined encode+write progress bar knows the write phase's size before
    /// `Par2WriteStarted` fires. Without it the bar's denominator would grow
    /// when writing begins, jumping the fraction backward. Usually equal to
    /// `par2_write_total`; kept separately because the write total only
    /// becomes authoritative at `Par2WriteStarted`.
    par2_recovery_total: usize,
    /// Number of input passes the encoder will make (`Par2EncodeStarted`).
    ///
    /// A tight memory budget splits the recovery set across several passes,
    /// and *each pass re-reads every input slice* — see `poster`'s pass loop,
    /// where the `par2_slices_fed` counter behind
    /// [`ProgressEvent::Par2InputProgress`] is declared inside the loop and so
    /// restarts at 0 every pass. Tracking the pass count (and
    /// `par2_pass_index` below) is what lets the panel present that repeated
    /// work as one monotonic progression instead of a bar that snaps back to
    /// zero once per pass.
    par2_passes: usize,
    /// Zero-based index of the pass currently being fed, inferred from
    /// `Par2InputProgress.done` going backwards (the event carries no pass
    /// number of its own).
    par2_pass_index: usize,
    /// `par2_write_done` at the last tick and a smoothed (EMA) slices/sec
    /// rate derived from it — see `par2_write_remaining_secs` for why a
    /// recent rate is used instead of the cumulative since-start average.
    prev_par2_write_done: u32,
    par2_write_rate_ema: f64,
    // Streaming check queue — runs concurrently with the upload, so there is
    // no fixed total known upfront (unlike the old end-of-run STAT sweep).
    check_active: bool,
    check_checked: u64,
    check_failed: u64,
    check_reposted: u64,
    check_start: Instant,
    /// Most recent retry backoff still in its window: (label without the
    /// countdown, e.g. "connection error — retry 1/3", deadline). Cleared
    /// once the deadline passes (`expire_check_retry`) rather than on every
    /// resolved article, so a fast-moving pool of concurrent check workers
    /// doesn't wipe it before the user can read it.
    check_retry: Option<(String, Instant)>,
    // PAR2 input slice encode progress
    par2_encode_done: usize,
    par2_encode_total: usize,
    par2_encode_start: Instant,
    /// `par2_encode_done` at the last tick and a smoothed (EMA) slices/sec
    /// rate derived from it — see `par2_encode_remaining_secs`.
    prev_par2_encode_done: usize,
    par2_encode_rate_ema: f64,
}

impl RenderState {
    fn new() -> Self {
        Self {
            started: false,
            finished: false,
            mode: RunMode::Post,
            target: None,
            start: Instant::now(),
            total_segments: 0,
            total_bytes: 0,
            done_segments: 0,
            done_bytes: 0,
            failures: 0,
            interrupted: false,
            status: String::new(),
            status_since: None,
            plain_status_printed: String::new(),
            plain_connections_printed: false,
            check_connections: 0,
            conn_files: Vec::new(),
            files: HashMap::new(),
            lines_drawn: 0,
            plain_ticks: 0,
            compress_active: false,
            compress_total: 0,
            compress_written: 0,
            compress_start: Instant::now(),
            par2_write_active: false,
            par2_write_total: 0,
            par2_write_done: 0,
            par2_write_start: Instant::now(),
            prev_par2_write_done: 0,
            par2_write_rate_ema: 0.0,
            par2_recovery_total: 0,
            par2_passes: 1,
            par2_pass_index: 0,
            check_active: false,
            check_checked: 0,
            check_failed: 0,
            check_reposted: 0,
            check_start: Instant::now(),
            check_retry: None,
            par2_encode_done: 0,
            par2_encode_total: 0,
            par2_encode_start: Instant::now(),
            prev_par2_encode_done: 0,
            par2_encode_rate_ema: 0.0,
            proc_rss_bytes: 0,
            proc_cpu_pct: 0.0,
            proc_prev_ticks: 0,
            proc_prev_tick_time: Instant::now(),
            speed_history: [0.0; 10],
            speed_history_pos: 0,
            speed_history_len: 0,
            prev_done_bytes: 0,
            spinner_frame: 0,
            conn_state: Vec::new(),
            buf_total: 0,
            buf_free: 0,
            par2_hint_remaining: 0,
            par2_segment_hint_remaining: 0,
            posting_par2: false,
            verbose: tracing::enabled!(tracing::Level::INFO),
        }
    }

    fn apply(&mut self, ev: ProgressEvent) {
        match ev {
            ProgressEvent::Started {
                mode,
                files,
                connections,
                check_connections,
                target,
                par2_bytes_hint,
                par2_segments_hint,
            } => {
                self.started = true;
                self.mode = mode;
                self.target = target;
                self.start = Instant::now();
                self.check_connections = check_connections;
                self.conn_files = vec![None; connections];
                self.conn_state = vec![ConnState::Idle; connections];
                for f in files {
                    self.total_segments += f.segments;
                    self.total_bytes += f.bytes;
                    self.files.insert(f.name, (0, f.segments));
                }
                // Pre-seed totals with the exact PAR2 geometry so neither
                // bar jumps when QueueExtended arrives with the real files.
                self.total_bytes += par2_bytes_hint;
                self.par2_hint_remaining = par2_bytes_hint;
                self.total_segments += par2_segments_hint;
                self.par2_segment_hint_remaining = par2_segments_hint;
            }
            ProgressEvent::ConnectionBusy { conn, file } => {
                if let Some(slot) = self.conn_files.get_mut(conn) {
                    *slot = Some(file);
                }
                if let Some(s) = self.conn_state.get_mut(conn) {
                    *s = ConnState::Busy;
                }
            }
            ProgressEvent::ConnectionIdle { conn } => {
                if let Some(slot) = self.conn_files.get_mut(conn) {
                    *slot = None;
                }
                if let Some(s) = self.conn_state.get_mut(conn) {
                    *s = ConnState::Idle;
                }
            }
            ProgressEvent::ConnectionAuth { conn } => {
                if let Some(s) = self.conn_state.get_mut(conn) {
                    *s = ConnState::Auth;
                }
            }
            ProgressEvent::ConnectionRetrying { conn } => {
                if let Some(s) = self.conn_state.get_mut(conn) {
                    *s = ConnState::Retrying;
                }
            }
            ProgressEvent::BufferPoolStats { total, free } => {
                self.buf_total = total;
                self.buf_free = free;
            }
            ProgressEvent::SegmentDone { file, bytes, ok } => {
                self.done_segments += 1;
                self.done_bytes += bytes;
                if !ok {
                    self.failures += 1;
                }
                if let Some(entry) = self.files.get_mut(&file) {
                    entry.0 += 1;
                }
            }
            ProgressEvent::QueueExtended {
                file,
                segments,
                bytes,
            } => {
                self.posting_par2 = true;
                // Absorb the real bytes/segments against the pre-seeded hints
                // so neither total jumps. If the real PAR2 geometry somehow
                // differs from the hint (slice-size/count overrides changing
                // between the estimate and the run shouldn't happen, but stay
                // defensive), only the excess grows the total.
                if bytes <= self.par2_hint_remaining {
                    self.par2_hint_remaining -= bytes;
                    // total_bytes already includes this; no change needed.
                } else {
                    let excess = bytes - self.par2_hint_remaining;
                    self.par2_hint_remaining = 0;
                    self.total_bytes += excess;
                }
                if segments <= self.par2_segment_hint_remaining {
                    self.par2_segment_hint_remaining -= segments;
                } else {
                    let excess = segments - self.par2_segment_hint_remaining;
                    self.par2_segment_hint_remaining = 0;
                    self.total_segments += excess;
                }
                self.files.entry(file).or_insert((0, 0)).1 += segments;
            }
            ProgressEvent::Status { text } => {
                if text.is_empty() {
                    self.status_since = None;
                } else if self.status.is_empty() || self.status != text {
                    self.status_since = Some(Instant::now());
                }
                self.status = text;
            }
            ProgressEvent::Failed { .. } => {}
            ProgressEvent::Interrupted => self.interrupted = true,
            ProgressEvent::Finished => self.finished = true,
            ProgressEvent::CompressStarted { total_bytes } => {
                self.compress_active = true;
                self.compress_total = total_bytes;
                self.compress_written = 0;
                self.compress_start = Instant::now();
                self.started = true; // allow panel to draw before posting starts
            }
            ProgressEvent::CompressProgress { bytes_written } => {
                self.compress_written = bytes_written;
            }
            ProgressEvent::CompressDone => {
                self.compress_written = self.compress_total;
                self.compress_active = false;
            }
            // Only the slice counts drive the panel. The rest of the encoder
            // geometry (slice size, chunking, SIMD path, memory budget) used
            // to be printed as a six-line block here; it is diagnostic detail
            // rather than progress, and `poster` already logs the same
            // numbers under `-v` ("PAR2 geometry" / "RS encoder").
            ProgressEvent::Par2EncodeStarted {
                input_slices,
                recovery_slices,
                passes,
                ..
            } => {
                self.par2_encode_total = input_slices;
                self.par2_encode_done = 0;
                self.par2_encode_start = Instant::now();
                self.prev_par2_encode_done = 0;
                self.par2_encode_rate_ema = 0.0;
                // Learn the write phase's size now so the combined bar has a
                // stable denominator before `Par2WriteStarted` arrives.
                self.par2_recovery_total = recovery_slices;
                self.par2_passes = passes.max(1);
                self.par2_pass_index = 0;
            }
            ProgressEvent::Par2InputProgress { done, total } => {
                // `done` restarting below the last value means the encoder
                // moved on to the next input pass (the event carries no pass
                // number). Count the completed pass so the panel's progress
                // keeps climbing instead of snapping back to zero.
                if done < self.par2_encode_done {
                    self.par2_pass_index =
                        (self.par2_pass_index + 1).min(self.par2_passes.saturating_sub(1));
                }
                self.par2_encode_done = done;
                self.par2_encode_total = total;
            }
            ProgressEvent::Par2WriteStarted { total } => {
                self.par2_write_active = true;
                self.par2_write_total = total;
                self.par2_write_done = 0;
                self.par2_write_start = Instant::now();
                self.prev_par2_write_done = 0;
                self.par2_write_rate_ema = 0.0;
            }
            ProgressEvent::Par2SliceWritten => {
                self.par2_write_done = self.par2_write_done.saturating_add(1);
                if self.par2_write_done >= self.par2_write_total {
                    self.par2_write_active = false;
                }
            }
            ProgressEvent::CheckProgress { checked, ok } => {
                if !self.check_active {
                    // Lazy start: the streaming check queue has no fixed
                    // total known upfront, so it just starts showing up the
                    // first time an article gets resolved, concurrently
                    // with the upload panel above it.
                    self.started = true;
                    self.check_active = true;
                    self.check_start = Instant::now();
                }
                self.check_checked = checked;
                if !ok {
                    self.check_failed += 1;
                }
            }
            ProgressEvent::CheckRetrying {
                attempt,
                max_attempts,
                delay_secs,
                reason,
            } => {
                self.check_retry = Some((
                    format!("{reason} — retry {attempt}/{max_attempts}"),
                    Instant::now() + Duration::from_secs(delay_secs),
                ));
            }
            ProgressEvent::CheckReposted { reposted } => {
                self.check_reposted = reposted;
            }
            ProgressEvent::CheckDone { failed } => {
                self.check_active = false;
                self.check_failed = failed;
                self.check_retry = None;
            }
        }
    }

    /// Files that have every segment done, and files partially in flight.
    fn file_tally(&self) -> (usize, usize) {
        let mut done = 0;
        let mut in_flight = 0;
        for &(d, total) in self.files.values() {
            if total > 0 && d >= total {
                done += 1;
            } else if d > 0 {
                in_flight += 1;
            }
        }
        (done, in_flight)
    }

    fn elapsed_secs(&self) -> f64 {
        self.start.elapsed().as_secs_f64().max(0.001)
    }

    /// Fraction of the run completed, measured in segments.
    ///
    /// Segments — not bytes — are the single source of truth for every
    /// percentage on screen. `total_bytes` carries the pre-seeded
    /// `par2_bytes_hint`, whose unconsumed remainder means a byte ratio tops
    /// out slightly short of 1.0 (`-q` visibly froze at 95% while the panel
    /// read `100%  864/864 seg`). Bytes stay the basis for speed, size and
    /// ETA, where that remainder is harmless.
    fn progress_frac(&self) -> f64 {
        if self.total_segments == 0 {
            return 0.0;
        }
        (self.done_segments as f64 / self.total_segments as f64).clamp(0.0, 1.0)
    }

    /// Bytes posted per second so far.
    fn rate(&self) -> f64 {
        self.done_bytes as f64 / self.elapsed_secs()
    }

    /// Record a per-tick speed sample in the ring buffer (phase 21c/21d).
    fn push_speed_sample(&mut self, bps: f64) {
        self.speed_history[self.speed_history_pos] = bps;
        self.speed_history_pos = (self.speed_history_pos + 1) % 10;
        if self.speed_history_len < 10 {
            self.speed_history_len += 1;
        }
    }

    /// Return the active speed history slice in chronological order.
    fn speed_samples(&self) -> Vec<f64> {
        let n = self.speed_history_len;
        if n == 0 {
            return Vec::new();
        }
        let start = if n < 10 {
            0
        } else {
            self.speed_history_pos // oldest slot when buffer is full
        };
        (0..n)
            .map(|i| self.speed_history[(start + i) % 10])
            .collect()
    }

    /// Compute ETA as a range based on throughput confidence (phase 21d).
    ///
    /// Returns `(low_secs, high_secs, unstable)`.
    fn eta_range(&self) -> Option<(f64, f64, bool)> {
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
        let variance =
            samples.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / samples.len() as f64;
        let sigma = variance.sqrt();
        let cv = sigma / mean;

        let mid = remaining / mean;
        if cv < 0.1 {
            return Some((mid, mid, false));
        }
        let low = remaining / (mean + sigma).max(1.0);
        // Clamp high to 10× low so instability never produces absurd ranges.
        // When sigma ≥ mean the lower-bound divisor approaches zero, which
        // would otherwise yield millions of hours.
        let high = (remaining / (mean - sigma).max(1.0)).min(low * 10.0);
        Some((low, high, cv >= 0.3))
    }

    /// Update the smoothed (EMA) PAR2 encode/write rates from this tick's
    /// delta. Called once per draw tick (~200ms), mirroring the byte-rate
    /// sampling right above its call site.
    ///
    /// The remaining-time estimates below deliberately use this smoothed
    /// recent rate rather than the cumulative since-start average: PAR2
    /// encoding is fed by the same data-starved read loop as the upload
    /// (see the module's architecture notes), so its progress is bursty —
    /// a slow start (or a network stall mid-run) skews a cumulative average
    /// for a long time afterward, swinging the displayed ETA wildly as the
    /// average slowly catches up. An EMA of the recent per-tick rate reacts
    /// in seconds instead of minutes.
    fn update_par2_rate_emas(&mut self) {
        const EMA_ALPHA: f64 = 0.15;
        if self.par2_encode_total > 0 {
            // Cumulative across passes, so a pass rollover (where the raw
            // `done` restarts at 0) doesn't register as a stalled tick.
            let units = self.par2_encode_units_done();
            let delta = units.saturating_sub(self.prev_par2_encode_done) as f64;
            let instant_rate = delta * (1000.0 / 200.0);
            self.prev_par2_encode_done = units;
            self.par2_encode_rate_ema = if self.par2_encode_rate_ema <= 0.0 {
                instant_rate
            } else {
                EMA_ALPHA * instant_rate + (1.0 - EMA_ALPHA) * self.par2_encode_rate_ema
            };
        }
        if self.par2_write_total > 0 {
            let delta = self
                .par2_write_done
                .saturating_sub(self.prev_par2_write_done) as f64;
            let instant_rate = delta * (1000.0 / 200.0);
            self.prev_par2_write_done = self.par2_write_done;
            self.par2_write_rate_ema = if self.par2_write_rate_ema <= 0.0 {
                instant_rate
            } else {
                EMA_ALPHA * instant_rate + (1.0 - EMA_ALPHA) * self.par2_write_rate_ema
            };
        }
    }

    /// Input slices fed so far across *every* pass. See [`Self::par2_passes`]:
    /// a multi-pass encode re-reads the whole input per pass and restarts its
    /// per-pass counter, so this is the only figure that rises monotonically.
    fn par2_encode_units_done(&self) -> usize {
        self.par2_pass_index * self.par2_encode_total + self.par2_encode_done
    }

    /// Total input-slice feeds the encode will perform across every pass.
    fn par2_encode_units_total(&self) -> usize {
        self.par2_passes.max(1) * self.par2_encode_total
    }

    /// Projected remaining seconds for the PAR2 encode phase, if it's active
    /// and has a usable rate estimate. PAR2 encoding runs concurrently with
    /// posting and can outlast it (e.g. a slow encode on a fast link, or
    /// extra passes forced by a tight memory budget) — folding this into the
    /// overall ETA (see its call site) means a slow encode shows up there
    /// instead of only in its own easy-to-miss indicator line. Counted over
    /// all passes, so a 3-pass encode isn't reported as nearly done at the end
    /// of pass 1.
    fn par2_encode_remaining_secs(&self) -> Option<f64> {
        let (done, total) = (
            self.par2_encode_units_done(),
            self.par2_encode_units_total(),
        );
        if total == 0 || done >= total {
            return None;
        }
        (self.par2_encode_rate_ema > 0.01)
            .then(|| (total - done) as f64 / self.par2_encode_rate_ema)
    }

    /// Same idea as [`Self::par2_encode_remaining_secs`], for the (usually
    /// short) phase that writes already-computed recovery data to disk.
    fn par2_write_remaining_secs(&self) -> Option<f64> {
        if self.par2_write_total == 0 || self.par2_write_done >= self.par2_write_total {
            return None;
        }
        (self.par2_write_rate_ema > 0.01).then(|| {
            (self.par2_write_total - self.par2_write_done) as f64 / self.par2_write_rate_ema
        })
    }

    /// Overall ETA in seconds, folding in PAR2 encode/write remaining time
    /// alongside the upload-side estimate — one pessimistic number instead of
    /// several separate, easily-conflicting ETAs on screen at once. Both
    /// folded phases run concurrently with the upload, so `max` (not a sum) is
    /// the right combinator: the run ends when the slowest of them does.
    ///
    /// The streaming check is deliberately *not* folded in. Its throughput is
    /// bimodal — throttled to the upload's pace while data is still going out,
    /// then bursting once the connections free up — so any rate extrapolation
    /// swings wildly right when the upload finishes (the very moment the ETA
    /// is read most). Its progress is already visible as the blue band inside
    /// the upload bar and the live tally in the check box, so no numeric
    /// estimate is needed for it.
    ///
    /// Returns `(seconds, unstable)`; `unstable` only ever reflects the
    /// upload-side estimate (`eta_range`), the only one with enough samples to
    /// judge confidence.
    fn overall_eta_secs(&self) -> Option<(f64, bool)> {
        let (mut best, unstable) = match self.eta_range() {
            Some((_lo, hi, u)) => (Some(hi), u),
            None => {
                let rate = self.rate();
                let fallback = (rate > 1.0 && self.total_bytes > self.done_bytes)
                    .then(|| (self.total_bytes - self.done_bytes) as f64 / rate);
                (fallback, false)
            }
        };
        for x in [
            self.par2_encode_remaining_secs(),
            self.par2_write_remaining_secs(),
        ]
        .into_iter()
        .flatten()
        {
            best = Some(best.map_or(x, |b: f64| b.max(x)));
        }
        best.map(|secs| (secs, unstable))
    }

    /// Draw quiet single-line mode (phase 21f).
    fn draw_quiet(&mut self, final_draw: bool) {
        if !self.started {
            return;
        }
        const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let spinner = if final_draw {
            '✓'
        } else {
            let ch = SPINNER[self.spinner_frame % SPINNER.len()];
            self.spinner_frame += 1;
            ch
        };

        let pct = (self.progress_frac() * 100.0).round() as u64;

        let eta_str = if final_draw {
            format!("done {}", format_duration(self.elapsed_secs()))
        } else if let Some((secs, unstable)) = self.overall_eta_secs() {
            let mark = if unstable { "~" } else { "" };
            format!("ETA {mark}{}", format_duration(secs))
        } else {
            "ETA —".to_string()
        };

        let width = terminal_width().unwrap_or(80).max(20);
        let line = truncate(&format!("{spinner}  {pct:>3}% · {eta_str}"), width);

        let mut out = String::new();
        if self.lines_drawn > 0 {
            // Cursor Previous Line (`\x1b[nF`) is supposed to move up *and*
            // return to column 1, but several terminals (some SSH clients,
            // minimal emulators) implement only plain Cursor Up (`\x1b[nA`),
            // leaving the column wherever it was. Do the column reset
            // ourselves with a literal `\r` — a bare control character every
            // terminal honours — instead of relying on `F`'s column-reset
            // semantics. Without this, a terminal that only supports `A`
            // never returns to column 0, so each redraw's text lands right
            // after the previous one instead of overwriting it.
            out.push_str("\x1b[1A\r\x1b[2K");
        }
        out.push_str(&line);
        out.push('\n');
        self.lines_drawn = 1;

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }

    // ---- TTY panel rendering --------------------------------------------

    /// Read RSS and CPU usage from /proc/self (Linux only; no-op on other OS).
    fn poll_proc_stats(&mut self) {
        #[cfg(target_os = "linux")]
        {
            // RSS from /proc/self/status  →  VmRSS: N kB
            if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                for line in status.lines() {
                    if let Some(rest) = line.strip_prefix("VmRSS:") {
                        if let Some(kb_str) = rest.split_whitespace().next() {
                            if let Ok(kb) = kb_str.parse::<u64>() {
                                self.proc_rss_bytes = kb * 1024;
                            }
                        }
                        break;
                    }
                }
            }
            // CPU from /proc/self/stat  →  field 14 (utime) + field 15 (stime)
            if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
                // Skip past the comm field which may contain spaces inside parens.
                let after_comm = stat.rfind(')').map(|i| &stat[i + 2..]).unwrap_or("");
                let fields: Vec<&str> = after_comm.split_whitespace().collect();
                // Fields are 0-indexed from after comm; utime is index 11, stime 12.
                if fields.len() > 12 {
                    let utime: u64 = fields[11].parse().unwrap_or(0);
                    let stime: u64 = fields[12].parse().unwrap_or(0);
                    let ticks = utime + stime;
                    let now = Instant::now();
                    let elapsed = now
                        .duration_since(self.proc_prev_tick_time)
                        .as_secs_f64()
                        .max(0.001);
                    let clk_tck: f64 = 100.0; // sysconf(_SC_CLK_TCK) is 100 on Linux
                    let delta_ticks = ticks.saturating_sub(self.proc_prev_ticks) as f64;
                    self.proc_cpu_pct = (delta_ticks / clk_tck / elapsed * 100.0).min(9999.0);
                    self.proc_prev_ticks = ticks;
                    self.proc_prev_tick_time = now;
                }
            }
        }
    }

    /// Clear a retry countdown once its deadline has passed, so the panel
    /// reverts to the normal "elapsed" line instead of freezing on "in 0s".
    fn expire_check_retry(&mut self) {
        if let Some((_, deadline)) = &self.check_retry {
            if Instant::now() >= *deadline {
                self.check_retry = None;
            }
        }
    }

    fn draw_panel(&mut self, final_draw: bool) {
        if !self.started {
            return;
        }
        // Record a speed sample for sparkline + ETA confidence (phase 21c/21d).
        let current_bps =
            self.done_bytes.saturating_sub(self.prev_done_bytes) as f64 * (1000.0 / 200.0); // per-tick delta → bytes/s (200 ms tick)
        self.prev_done_bytes = self.done_bytes;
        if !final_draw && self.done_bytes > 0 {
            self.push_speed_sample(current_bps);
        }
        if !final_draw {
            self.update_par2_rate_emas();
        }
        if !final_draw && self.verbose {
            self.poll_proc_stats();
        }
        self.expire_check_retry();
        // Every emitted line must fit within one physical terminal row, or
        // it wraps onto a second row the redraw logic below doesn't know
        // about: cursor-up moves by *logical* line count, so once any line
        // wraps, moving up N logical lines undershoots the true top of the
        // previous frame, leaving a stray fragment behind on every redraw
        // (reported as the header line — the one line not bounded by the
        // fixed-width box below it — repeating itself over and over on
        // narrow terminals, e.g. a phone SSH client). Truncating every line
        // to the actual detected width keeps the 1-logical-line-per-row
        // invariant the cursor arithmetic below depends on.
        let width = terminal_width().unwrap_or(80).max(20);
        let lines: Vec<String> = self
            .panel_lines(final_draw, width)
            .into_iter()
            .map(|l| truncate(&l, width))
            .collect();

        let mut out = String::new();
        // Move the cursor back to the top of the previous panel and wipe
        // everything below it, so a shorter panel leaves no stale lines.
        //
        // Cursor Previous Line (`\x1b[nF`) is supposed to move up *and*
        // return to column 1, but several terminals (some SSH clients,
        // minimal emulators) implement only plain Cursor Up (`\x1b[nA`),
        // leaving the column wherever the previous draw left it. Do the
        // column reset ourselves with a literal `\r` instead of relying on
        // `F`'s column-reset semantics — without this, a terminal that only
        // supports `A` never returns to column 0, so `\x1b[0J`'s erase
        // starts mid-line and every redraw's text lands right after the
        // previous one instead of overwriting it (looks like the header
        // line repeating itself dozens of times instead of updating).
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

    /// The two-line run summary that replaces the live panel once the run is
    /// over. Deliberately small: the binary follows it with the `wrote
    /// nzb`/`wrote nfo` paths, so this covers only what the renderer itself
    /// knows — outcome, throughput and verification.
    fn summary_lines(&self, width: usize) -> Vec<String> {
        let ok = self.failures == 0 && self.check_failed == 0 && !self.interrupted;
        let glyph = if self.interrupted {
            ansi("⚠", "33")
        } else if ok {
            ansi("✓", "32")
        } else {
            ansi("✗", "31")
        };
        let avg = self.done_bytes as f64 / self.elapsed_secs();
        let mode_note = match self.mode {
            RunMode::Post => String::new(),
            RunMode::DryRun => format!(" · {}", ansi("dry run", "33")),
            RunMode::Par2Only => format!(" · {}", ansi("par2 only", "36")),
        };
        let line1 = format!(
            "{glyph} {} in {} · avg {}/s{mode_note}",
            format_size(self.done_bytes),
            format_duration(self.elapsed_secs()),
            format_size(avg as u64),
        );

        let mut parts = vec![format!(
            "{}/{} seg",
            self.done_segments, self.total_segments
        )];
        if self.failures > 0 {
            parts.push(ansi(&format!("{} failed", self.failures), "31"));
        }
        if self.check_checked > 0 || self.check_failed > 0 {
            if self.check_failed > 0 {
                parts.push(ansi(&format!("{} missing", self.check_failed), "31"));
            } else {
                parts.push(ansi("all verified", CHECK_BAND_COLOR));
            }
        }
        if self.check_reposted > 0 {
            parts.push(format!("{} reposted", self.check_reposted));
        }
        let line2 = format!("  {}", parts.join(" · "));

        vec![line1, line2]
            .into_iter()
            .map(|l| truncate(&l, width))
            .collect()
    }

    /// Erase the live panel and print the compact final summary in its place.
    fn draw_summary(&mut self) {
        if !self.started {
            return;
        }
        let width = terminal_width().unwrap_or(80).max(20);
        let mut out = String::new();
        // Same erase dance as `draw_panel` — move to the top of the last
        // frame and clear downward before writing the (shorter) summary.
        if self.lines_drawn > 0 {
            out.push_str(&format!("\x1b[{}A\r", self.lines_drawn));
        }
        out.push_str("\x1b[0J");
        for line in self.summary_lines(width) {
            out.push_str(&line);
            out.push('\n');
        }
        self.lines_drawn = 0;

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }

    /// Build the panel for a terminal `width` columns wide. Every box sizes
    /// itself off that width rather than a compile-time constant.
    fn panel_lines(&self, final_draw: bool, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let body_w = body_width(width);
        let bar_w = bar_width(body_w);

        // --- header with phase indicator and elapsed time ----------------
        let elapsed_hdr = if self.started {
            format!(" · {}", format_duration(self.elapsed_secs()))
        } else {
            String::new()
        };
        // The header names the phase that dominates the *remaining* work, in
        // priority order. Ordering matters: uploads and PAR2 encoding run
        // concurrently, so once the upload bar is full the label must move on
        // to whatever is actually still running (the streaming check, then any
        // trailing PAR2 write) instead of freezing on "posting PAR2" while the
        // connections sit idle — the old chain did exactly that, and also lied
        // with "writing PAR2" during the data pass just because a (0/N) write
        // phase had been announced.
        let uploading = !self.files.is_empty() && self.done_segments < self.total_segments;
        let phase = if self.compress_active && self.files.is_empty() {
            ansi("compressing", "33") // yellow
        } else if uploading && self.posting_par2 {
            ansi("posting PAR2", "35") // magenta
        } else if uploading {
            ansi("posting data", "32") // green
        } else if self.check_active {
            ansi("verifying", "34") // blue — matches the check bar's band
        } else if self.par2_write_active || (!self.files.is_empty() && !self.finished) {
            ansi("writing PAR2", "36") // cyan
        } else if self.finished {
            ansi("done", "32")
        } else {
            "starting".to_string()
        };
        let verb_suffix = match self.mode {
            RunMode::Post => String::new(),
            RunMode::DryRun => format!(" · {}", ansi("dry run", "33")),
            RunMode::Par2Only => format!(" · {}", ansi("par2 only", "36")),
        };
        let file_count = self.files.len();
        let target_str = self
            .target
            .as_deref()
            .map(|t| format!(" → {t}"))
            .unwrap_or_default();
        lines.push(format!(
            "pesto  {phase}  {file_count} file(s){target_str}{verb_suffix}{elapsed_hdr}"
        ));

        // --- compression box (shown while compressing) -------------------
        if self.compress_active || (final_draw && self.compress_total > 0 && self.files.is_empty())
        {
            let elapsed = self.compress_start.elapsed().as_secs_f64().max(0.001);
            let frac = if self.compress_total > 0 {
                (self.compress_written as f64 / self.compress_total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let pct = (frac * 100.0).round() as u64;
            let bar = render_bar(frac, bar_w);
            let rate = self.compress_written as f64 / elapsed;
            let line1 = format!(
                "[{bar}] {pct:>3}%  {}/{}",
                format_size(self.compress_written),
                format_size(self.compress_total),
            );
            let eta = if final_draw {
                format!("elapsed {}", format_duration(elapsed))
            } else if rate > 1.0 && self.compress_total > self.compress_written {
                let remaining = (self.compress_total - self.compress_written) as f64 / rate;
                format!("ETA {}", format_duration(remaining))
            } else {
                "ETA —".to_string()
            };
            let line2 = format!("{}/s · {eta}", format_size(rate as u64));
            lines.push(box_top("compressing", body_w));
            lines.push(box_line(&line1, body_w));
            lines.push(box_line(&line2, body_w));
            lines.push(box_bottom(body_w));
        }

        // --- overall posting box (only after posting has started) --------
        if !self.files.is_empty() {
            // Segment ratio, shared with `-q` — see `progress_frac`.
            let frac = self.progress_frac();
            let pct = (frac * 100.0).round() as u64;
            // Trailing band: how much of the plan the streaming check queue
            // has already confirmed, on the same 0..=total_segments scale as
            // the upload's own leading edge — see `render_dual_bar`.
            let checked_frac = if self.total_segments > 0 {
                (self.check_checked as f64 / self.total_segments as f64).clamp(0.0, frac)
            } else {
                0.0
            };
            let bar = render_dual_bar(checked_frac, frac, bar_w);
            // Line 1: bar + percentage + segment count
            let line1 = format!(
                "[{bar}] {pct:>3}%  {}/{} seg",
                self.done_segments, self.total_segments
            );
            let rate = self.rate();
            let (line2, line3) = if final_draw {
                // On final draw: show total size, average speed, and elapsed time.
                // Average speed is more meaningful than the last-tick instantaneous rate.
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
                // While uploading: bytes/total · instantaneous speed · sparkline
                let spark = {
                    let samples = self.speed_samples();
                    // Suppress sparkline on narrow terminals (< 60 columns) to
                    // avoid truncating the speed/size figures that matter more.
                    let wide_enough = width >= 60;
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
                let l3 = match self.overall_eta_secs() {
                    Some((secs, unstable)) => {
                        let mark = if unstable { "~" } else { "" };
                        format!("ETA {mark}{}", format_duration(secs))
                    }
                    None => "ETA —".to_string(),
                };
                (l2, Some(l3))
            };
            lines.push(box_top("upload", body_w));
            lines.push(box_line(&line1, body_w));
            lines.push(box_line(&line2, body_w));
            if let Some(l3) = line3 {
                lines.push(box_line(&l3, body_w));
            }
            lines.push(box_bottom(body_w));

            // --- streaming check queue box ---------------------------------
            // Runs concurrently with the upload above (its own dedicated
            // connections, started a few seconds after the first segment
            // posts). It has no bar of its own on purpose: the upload bar
            // already paints check progress as its trailing blue band (see
            // `render_dual_bar`), so a second bar here just duplicated it.
            // This box carries the numbers that band can't — the running
            // verified / pending / missing / reposted tally.
            if self.check_active || (final_draw && self.check_checked > 0) {
                let verified = self.check_checked.saturating_sub(self.check_failed);
                let pending = self.done_segments.saturating_sub(self.check_checked);
                // Colour-match the upload bar's two bands: "verified" in the
                // checked band's colour, "pending" in the upload band's —
                // a legend for the bar that lives right next to the numbers
                // it explains instead of a separate line or header colouring.
                let verified_str = ansi(&format!("{verified} verified"), CHECK_BAND_COLOR);
                let pending_str = ansi(&format!("{pending} pending"), UPLOAD_BAND_COLOR);
                let mut line1 = if self.check_failed > 0 {
                    format!(
                        "{verified_str} · {pending_str} · {}",
                        ansi(&format!("{} missing", self.check_failed), "31")
                    )
                } else if !self.check_active {
                    format!("{verified_str} · all confirmed")
                } else {
                    format!("{verified_str} · {pending_str}")
                };
                // Reposts had no counter at all before — the only trace was
                // a `Status` line shared (and instantly overwritten) by
                // every other kind of status message in the app.
                if self.check_reposted > 0 {
                    line1.push_str(&format!(
                        " · {}",
                        ansi(&format!("{} reposted", self.check_reposted), "33")
                    ));
                }
                // No ETA here: the check's throughput jumps once the upload
                // frees its connections, so any estimate would swing wildly
                // (see `overall_eta_secs`). Elapsed time is honest and steady.
                let elapsed = self.check_start.elapsed().as_secs_f64().max(0.001);
                let line2 = if let Some((label, deadline)) = &self.check_retry {
                    let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
                    format!("{label} in {remaining}s")
                } else {
                    format!("elapsed {}", format_duration(elapsed))
                };
                lines.push(box_top("check", body_w));
                lines.push(box_line(&line1, body_w));
                lines.push(box_line(&line2, body_w));
                lines.push(box_bottom(body_w));
            }

            // --- per-connection activity as a single dot row --------------
            // Was a two-per-line grid of `conn N ▸ file` cells: up to six rows
            // that, since every worker posts the *same* file, repeated one
            // truncated name N times and then sat as N rows of `idle` for the
            // whole check phase. A row of state-coloured dots carries the same
            // information (how many busy / retrying / idle) in one fixed line,
            // and stops the panel's height from lurching as workers drain.
            let conns = self.conn_files.len();
            if conns > 0 {
                let total_conns = conns + self.check_connections;
                let active = self.conn_files.iter().filter(|c| c.is_some()).count();
                let retrying = self
                    .conn_state
                    .iter()
                    .filter(|&&s| s == ConnState::Retrying)
                    .count();
                // Only draw an individual dot per connection while the count
                // stays legible; above that a plain tally reads better than a
                // wall of dots that would wrap or get truncated.
                let dots = if conns <= GRID_LIMIT {
                    let mut s = String::from("  ");
                    for st in &self.conn_state {
                        s.push_str(&conn_dot(*st));
                    }
                    s
                } else {
                    String::new()
                };
                let check_str = if self.check_connections > 0 {
                    format!(" · {} check", self.check_connections)
                } else {
                    String::new()
                };
                let retry_str = if retrying > 0 {
                    format!(" · {}", ansi(&format!("{retrying} retrying"), "31"))
                } else {
                    String::new()
                };
                lines.push(format!(
                    "conns{dots}  {total_conns} total · {active}/{conns} active{check_str}{retry_str}"
                ));
            }

            // --- file tally + failures -----------------------------------
            let (done, in_flight) = self.file_tally();
            let total_files = self.files.len();
            let pending = total_files.saturating_sub(done + in_flight);
            // Show the name of the file currently being uploaded (first busy conn).
            let active_file = self
                .conn_files
                .iter()
                .find_map(|f| f.as_deref())
                .map(|name| format!("  ▸ {}", truncate(name, 28)))
                .unwrap_or_default();
            let failures_str = if self.failures > 0 {
                format!("  {}", ansi(&format!("{} failed", self.failures), "31"))
            } else {
                String::new()
            };
            lines.push(format!(
                "files  done {done}/{total_files}  uploading {in_flight}  waiting {pending}{failures_str}{active_file}"
            ));

            // --- buffer pool visualizer (phase 21h, shown under pressure) -
            if self.buf_total > 0 && self.buf_free * 4 < self.buf_total {
                let frac_free = self.buf_free as f64 / self.buf_total as f64;
                let bar = render_bar(1.0 - frac_free, 10);
                let buf_line = format!(
                    "buf [{bar}] {}/{} used",
                    self.buf_total - self.buf_free,
                    self.buf_total,
                );
                lines.push(ansi(&buf_line, "33")); // yellow when under pressure
            }
        }

        // --- PAR2 secondary indicator (encode + write as one bar) ---------
        // Shown below the upload box as a single dim, indented line so the
        // upload bar stays the focal point. Encode and write were two separate
        // free-floating bars with inconsistent grammar (only encode had a
        // percentage) and fixed non-responsive widths; to the user, though,
        // "generate PAR2" is one activity with two internal stages that run
        // essentially back to back. One monotonic bar over the combined slice
        // work — with the active stage named inline — reads as that single
        // activity, keeps the panel a fixed height, and matches the responsive
        // `[bar] pct%` grammar of the boxes above. No ETA here: its remaining
        // time already feeds the single overall ETA (`overall_eta_secs`).
        // Encode work is counted over every pass (see `par2_encode_units_*`),
        // so a memory-constrained multi-pass encode — which re-reads the whole
        // input per pass — advances the bar smoothly instead of resetting it
        // to zero once per re-read.
        let encode_done = self.par2_encode_units_done();
        let encode_total = self.par2_encode_units_total();
        let par2_total = encode_total + self.par2_recovery_total;
        let par2_done = encode_done + self.par2_write_done as usize;
        if !final_draw && par2_total > 0 && par2_done < par2_total {
            let frac = (par2_done as f64 / par2_total as f64).clamp(0.0, 1.0);
            // Floor, not round: this line only renders while PAR2 work is
            // still outstanding, so a rounded 99.8% displaying as "100%"
            // would claim the stage is done while slices are still being
            // written (seen as `100%  write 258/273`).
            let pct = (frac * 100.0).floor() as u64;
            let bar = render_bar(frac, bar_w);
            // Name the stage actually running. Encode is the long pole and
            // completes before writing meaningfully starts, so prefer it while
            // it is still going; otherwise report the write flush.
            let stage = if encode_done < encode_total {
                // With several passes the per-pass counter alone is confusing
                // ("encode 12/655" three separate times), so name the pass.
                let pass_note = if self.par2_passes > 1 {
                    format!(" (pass {}/{})", self.par2_pass_index + 1, self.par2_passes)
                } else {
                    String::new()
                };
                format!(
                    "encode {}/{}{pass_note}",
                    self.par2_encode_done, self.par2_encode_total
                )
            } else {
                format!("write {}/{}", self.par2_write_done, self.par2_write_total)
            };
            lines.push(ansi(&format!("  par2  [{bar}] {pct:>3}%  {stage}"), "2"));
        }

        // --- process resource stats (Linux /proc/self, `-v` only) --------
        #[cfg(target_os = "linux")]
        if self.verbose && self.proc_rss_bytes > 0 {
            let rss = format_size(self.proc_rss_bytes);
            let cpu = format!("{:.1}%", self.proc_cpu_pct);
            let res_line = format!("process  ram {}  cpu {}", rss, cpu);
            lines.push(ansi(&res_line, "2")); // dim — informational, not critical
        }

        // --- optional status / interrupt note ----------------------------
        if self.interrupted {
            lines.push("⚠ interrupt received — finishing in-flight segments".to_string());
        } else if !self.status.is_empty() {
            let elapsed_str = if let Some(since) = self.status_since {
                format!(" · {}", format_duration(since.elapsed().as_secs_f64()))
            } else {
                String::new()
            };
            lines.push(format!("▸ {}{}", self.status, elapsed_str));
        }

        lines
    }

    // ---- non-TTY plain rendering ----------------------------------------

    fn draw_plain(&mut self, final_draw: bool) {
        if !self.started {
            return;
        }
        // Print once, right when the run starts — upload and check pools are
        // separate connection sets (see `split_connections`), so a single
        // combined number would hide how many of each are actually in use.
        if !self.plain_connections_printed {
            self.plain_connections_printed = true;
            let conns = self.conn_files.len();
            if conns > 0 || self.check_connections > 0 {
                let check_str = if self.check_connections > 0 {
                    format!(" · {} check", self.check_connections)
                } else {
                    String::new()
                };
                let total_conns = conns + self.check_connections;
                let mut err = std::io::stderr().lock();
                let _ = writeln!(
                    err,
                    "connections: {total_conns} total ({conns} upload{check_str})"
                );
                let _ = err.flush();
            }
        }
        // Throttle to roughly one line every ~2s so logs stay readable.
        self.plain_ticks += 1;
        if !final_draw && !self.plain_ticks.is_multiple_of(10) {
            return;
        }
        self.expire_check_retry();
        let mut err = std::io::stderr().lock();

        // Print each new `status` line exactly once, ahead of the
        // phase-specific branches below — several of those `return` early,
        // which used to mean a `Status` event (e.g. the memory-budget
        // banner, or "PAR2 recovery data split into N passes") never made it
        // into redirected/logged output at all.
        if !self.status.is_empty() && self.status != self.plain_status_printed {
            let _ = writeln!(err, "{}", self.status);
            let _ = err.flush();
            self.plain_status_printed = self.status.clone();
        }

        if self.compress_active {
            let elapsed = self.compress_start.elapsed().as_secs_f64().max(0.001);
            let rate = self.compress_written as f64 / elapsed;
            let _ = writeln!(
                err,
                "compressing: {}/{} · {}/s",
                format_size(self.compress_written),
                format_size(self.compress_total),
                format_size(rate as u64),
            );
            let _ = err.flush();
            return;
        }

        if self.par2_write_active {
            let _ = writeln!(
                err,
                "par2: {}/{} slices",
                self.par2_write_done, self.par2_write_total,
            );
            let _ = err.flush();
            return;
        }

        // Streaming check runs concurrently with the upload, so it's an
        // extra suffix on the normal progress line rather than a phase that
        // suppresses it.
        let check_suffix = if self.check_active || (final_draw && self.check_checked > 0) {
            let verified = self.check_checked.saturating_sub(self.check_failed);
            let pending = self.done_segments.saturating_sub(self.check_checked);
            let mut suffix = format!(
                " · check {verified} verified/{} missing/{pending} pending",
                self.check_failed
            );
            if self.check_reposted > 0 {
                suffix.push_str(&format!("/{} reposted", self.check_reposted));
            }
            // Unlike the boxed panel, plain mode has no dedicated retry line
            // — append it here so a connection-error backoff isn't silently
            // invisible when output is redirected/logged (non-TTY).
            if let Some((label, deadline)) = &self.check_retry {
                let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
                suffix.push_str(&format!(" · {label} in {remaining}s"));
            }
            suffix
        } else {
            String::new()
        };

        if self.files.is_empty() {
            if !check_suffix.is_empty() {
                let _ = writeln!(err, "check:{check_suffix}");
                let _ = err.flush();
            }
            return;
        }

        let rate = self.rate();
        if final_draw {
            let _ = writeln!(
                err,
                "done: {}/{} segments · {} · {} failures · {}{check_suffix}",
                self.done_segments,
                self.total_segments,
                format_size(self.done_bytes),
                self.failures,
                format_duration(self.elapsed_secs()),
            );
        } else {
            let _ = writeln!(
                err,
                "posting: {}/{} segments · {} · {}/s{check_suffix}",
                self.done_segments,
                self.total_segments,
                format_size(self.done_bytes),
                format_size(rate as u64),
            );
        }
        let _ = err.flush();
    }
}

/// One cell of the connection grid with colour-coded state (phase 21b).
/// One state-coloured dot for the connection row: filled `●` when the worker
/// is doing something (busy/auth/retrying), hollow dim `○` when idle.
fn conn_dot(state: ConnState) -> String {
    match state {
        ConnState::Busy => ansi("●", "32"),     // green — posting
        ConnState::Auth => ansi("●", "33"),     // yellow — authenticating
        ConnState::Retrying => ansi("●", "31"), // red — retrying
        ConnState::Idle => ansi("○", "2"),      // dim — drained
    }
}

// Colours for the two upload-bar bands: the leading edge (segments posted,
// not yet check-confirmed) and the trailing edge (segments the streaming
// check queue has already resolved). Kept distinct from every other colour
// in use (see the `ansi(` call sites above) so the two meanings never blur
// into an existing one.
const UPLOAD_BAND_COLOR: &str = "32"; // green — matches the "posting data" phase label
const CHECK_BAND_COLOR: &str = "34"; // blue — segments confirmed retrievable via STAT

/// Draw a two-colour proportional bar: a `checked_frac` portion in
/// [`CHECK_BAND_COLOR`] (how much of the upload the streaming check queue has
/// already confirmed), followed by the rest of the `total_frac` portion in
/// [`UPLOAD_BAND_COLOR`] (posted but not yet confirmed), followed by the
/// unfilled `░` remainder — unchanged from [`render_bar`], so the bar's empty
/// state looks exactly as it always has.
///
/// `checked_frac` is always `<= total_frac` in practice (the check queue can
/// only confirm what has already been posted); this is enforced defensively
/// with `clamp` so an out-of-order event can never render a checked band
/// past the upload band's own leading edge.
fn render_dual_bar(checked_frac: f64, total_frac: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let checked_frac = checked_frac.clamp(0.0, 1.0);
    let total_frac = total_frac.clamp(checked_frac, 1.0);

    // The checked/upload split renders at whole-cell granularity; only the
    // outer (upload) leading edge gets the smooth sub-character treatment,
    // exactly like the single-colour bar.
    let checked_cells = ((checked_frac * width as f64).round() as usize).min(width);

    let total_eighths = (total_frac * width as f64 * 8.0).round() as usize;
    let total_full = (total_eighths / 8).min(width).max(checked_cells);
    let remainder = if total_full < width {
        total_eighths % 8
    } else {
        0
    };

    let mut checked_part = String::with_capacity(checked_cells * 3);
    for _ in 0..checked_cells {
        checked_part.push('█');
    }

    let mut upload_part = String::with_capacity((total_full - checked_cells + 1) * 3);
    for _ in checked_cells..total_full {
        upload_part.push('█');
    }
    if remainder > 0 {
        upload_part.push(SUBCHAR[remainder - 1]);
    }

    let filled_cells = if remainder > 0 {
        total_full + 1
    } else {
        total_full
    };
    let pending_cells = width.saturating_sub(filled_cells);

    let mut s = String::new();
    if !checked_part.is_empty() {
        s.push_str(&ansi(&checked_part, CHECK_BAND_COLOR));
    }
    if !upload_part.is_empty() {
        s.push_str(&ansi(&upload_part, UPLOAD_BAND_COLOR));
    }
    for _ in 0..pending_cells {
        s.push('░');
    }
    s
}

/// Human-readable byte size with binary (IEC) units.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::FileEntry;
    use crate::ui::render::visible_len;

    fn started_state(width_samples: bool) -> RenderState {
        let mut state = RenderState::new();
        state.apply(ProgressEvent::Started {
            mode: RunMode::Post,
            files: vec![FileEntry {
                name: "Movie.2026/movie.mkv".to_string(),
                segments: 785,
                bytes: 600_000_000,
            }],
            connections: 7,
            check_connections: 1,
            target: Some("news.example.com:563".to_string()),
            par2_bytes_hint: 60_000_000,
            par2_segments_hint: 79,
        });
        for _ in 0..300 {
            state.apply(ProgressEvent::SegmentDone {
                file: "Movie.2026/movie.mkv".to_string(),
                bytes: 768_000,
                ok: true,
            });
        }
        if width_samples {
            state.push_speed_sample(50_000_000.0);
            state.push_speed_sample(60_000_000.0);
        }
        state
    }

    #[test]
    fn box_borders_survive_the_terminal_width_truncation() {
        // The panel used to be a fixed 60 columns wide, so a narrower
        // terminal had `draw_panel`'s truncate eat the right-hand border of
        // every box, leaving `┌─ upload ────…`. Boxes must now be sized so
        // that same truncation is a no-op for them. Below `MIN_BODY_W + 4`
        // columns there is nothing to be done, so that is the floor.
        let state = started_state(true);
        for width in [MIN_BODY_W + 4, 30, 40, 50, 60, 80, 100, 140, 200] {
            for line in state.panel_lines(false, width) {
                let first = line.chars().next().unwrap_or(' ');
                if !"┌│└".contains(first) {
                    continue;
                }
                assert_eq!(
                    truncate(&line, width),
                    line,
                    "box line loses its right border at width={width}"
                );
                let last = line.chars().last().unwrap_or(' ');
                assert!(
                    "┐│┘".contains(last),
                    "width={width} left a ragged box line: {line:?}"
                );
            }
        }
    }

    #[test]
    fn box_borders_line_up_with_each_other() {
        let state = started_state(false);
        for width in [40, 60, 80, 120] {
            let lines = state.panel_lines(false, width);
            let box_lines: Vec<&String> = lines
                .iter()
                .filter(|l| l.starts_with('┌') || l.starts_with('│') || l.starts_with('└'))
                .collect();
            assert!(!box_lines.is_empty(), "no box drawn at width={width}");
            let expected = visible_len(box_lines[0]);
            for line in &box_lines {
                assert_eq!(
                    visible_len(line),
                    expected,
                    "ragged box edge at width={width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn bar_width_keeps_the_historical_ratio() {
        // 26-in-56 is what the fixed-width panel drew; keep it as the anchor
        // so the default 80-column terminal looks unchanged.
        assert_eq!(bar_width(56), 25);
        assert!(bar_width(24) >= 10);
        assert!(bar_width(MAX_BODY_W) <= 40);
    }

    #[test]
    fn quiet_and_panel_report_the_same_percentage() {
        // `-q` used to divide bytes (freezing at 95% because `total_bytes`
        // carries the unconsumed PAR2 hint) while the panel divided segments.
        let state = started_state(false);
        let pct_quiet = (state.progress_frac() * 100.0).round() as u64;
        let panel = state.panel_lines(false, 80);
        let bar_line = panel
            .iter()
            .find(|l| l.contains("seg"))
            .expect("upload box drawn");
        assert!(
            bar_line.contains(&format!("{pct_quiet}%")),
            "panel line {bar_line:?} disagrees with quiet {pct_quiet}%"
        );
    }

    #[test]
    fn progress_reaches_100_percent_when_every_segment_is_done() {
        let mut state = started_state(false);
        let remaining = state.total_segments - state.done_segments;
        for _ in 0..remaining {
            state.apply(ProgressEvent::SegmentDone {
                file: "Movie.2026/movie.mkv".to_string(),
                bytes: 768_000,
                ok: true,
            });
        }
        assert_eq!((state.progress_frac() * 100.0).round() as u64, 100);
    }

    #[test]
    fn par2_encoder_details_and_process_line_stay_out_of_the_panel() {
        let mut state = started_state(false);
        state.apply(ProgressEvent::Par2EncodeStarted {
            input_bytes: 600_000_000,
            input_slices: 785,
            input_files: 1,
            recovery_slices: 78,
            slice_size: 768_000,
            passes: 1,
            chunk_size: 32_768,
            simd_method: "avx2+gfni".to_string(),
            threads: 6,
            memory_limit: 16 << 30,
        });
        state.proc_rss_bytes = 200 << 20;
        let panel = state.panel_lines(false, 100).join("\n");
        assert!(
            panel.contains("par2  [") && panel.contains("encode 0/785"),
            "the combined par2 progress bar itself must stay:\n{panel}"
        );
        for noise in [
            "PAR2 encoder",
            "Multiply method",
            "Memory usage",
            "Input pass(es)",
            "process  ram",
        ] {
            assert!(
                !panel.contains(noise),
                "{noise:?} should not be in the default panel:\n{panel}"
            );
        }
    }

    #[test]
    fn par2_bar_is_one_line_and_never_regresses_across_encode_then_write() {
        let mut state = started_state(false);
        state.apply(ProgressEvent::Par2EncodeStarted {
            input_bytes: 600_000_000,
            input_slices: 100,
            input_files: 1,
            recovery_slices: 20,
            slice_size: 768_000,
            passes: 1,
            chunk_size: 32_768,
            simd_method: "avx2".to_string(),
            threads: 6,
            memory_limit: 16 << 30,
        });

        // Walk encode 0→100, then write 0→20, sampling the combined fraction.
        let mut last = -1.0_f64;
        let mut par2_line_counts = Vec::new();
        let mut sample = |st: &RenderState| {
            let panel = st.panel_lines(false, 100);
            let par2: Vec<&String> = panel.iter().filter(|l| l.contains("par2  [")).collect();
            par2_line_counts.push(par2.len());
        };

        for done in (0..=100).step_by(10) {
            state.apply(ProgressEvent::Par2InputProgress { done, total: 100 });
            sample(&state);
            let frac = (state.par2_encode_done + state.par2_write_done as usize) as f64
                / (state.par2_encode_total + state.par2_recovery_total) as f64;
            assert!(
                frac + 1e-9 >= last,
                "par2 fraction went backward: {frac} < {last}"
            );
            last = frac;
        }
        state.apply(ProgressEvent::Par2WriteStarted { total: 20 });
        for _ in 0..20 {
            state.apply(ProgressEvent::Par2SliceWritten);
            sample(&state);
            let frac = (state.par2_encode_done + state.par2_write_done as usize) as f64
                / (state.par2_encode_total + state.par2_recovery_total) as f64;
            assert!(
                frac + 1e-9 >= last,
                "par2 fraction went backward: {frac} < {last}"
            );
            last = frac;
        }

        // At most one par2 line at any sampled moment — never the old two.
        assert!(
            par2_line_counts.iter().all(|&n| n <= 1),
            "par2 should render as a single line, saw counts {par2_line_counts:?}"
        );
    }

    #[test]
    fn par2_bar_does_not_reset_on_a_multi_pass_encode() {
        // A tight memory budget splits the recovery set across passes, and
        // every pass re-reads the whole input: `poster` declares the
        // `Par2InputProgress` counter *inside* its pass loop, so `done`
        // restarts at 0 each time. Without pass accounting the bar snapped
        // back to zero once per pass.
        const SLICES: usize = 200;
        const PASSES: usize = 3;
        let mut state = started_state(false);
        state.apply(ProgressEvent::Par2EncodeStarted {
            input_bytes: 600_000_000,
            input_slices: SLICES,
            input_files: 1,
            recovery_slices: 60,
            slice_size: 768_000,
            passes: PASSES,
            chunk_size: 32_768,
            simd_method: "avx2".to_string(),
            threads: 6,
            memory_limit: 1 << 30,
        });

        let frac = |st: &RenderState| {
            (st.par2_encode_units_done() + st.par2_write_done as usize) as f64
                / (st.par2_encode_units_total() + st.par2_recovery_total) as f64
        };

        let mut last = 0.0_f64;
        for pass in 0..PASSES {
            for done in (0..=SLICES).step_by(20) {
                state.apply(ProgressEvent::Par2InputProgress {
                    done,
                    total: SLICES,
                });
                let f = frac(&state);
                assert!(
                    f + 1e-9 >= last,
                    "bar reset on pass {pass} at done={done}: {f} < {last}"
                );
                last = f;
            }
        }
        assert_eq!(
            state.par2_pass_index,
            PASSES - 1,
            "should have tracked every pass rollover"
        );
        // Encode complete across all passes → the bar sits at the encode
        // share, then the write stage carries it to 100%.
        assert_eq!(state.par2_encode_units_done(), SLICES * PASSES);

        // And the panel names the pass while multi-pass encoding is running.
        state.apply(ProgressEvent::Par2InputProgress {
            done: 10,
            total: SLICES,
        });
        let panel = state.panel_lines(false, 100).join("\n");
        assert!(
            panel.contains("pass 3/3"),
            "multi-pass encode should name the current pass:\n{panel}"
        );
    }

    #[test]
    fn par2_never_shows_100_percent_while_work_remains() {
        // The line only renders while PAR2 work is outstanding, so a rounded
        // 99.8% reading "100%" claimed the stage was finished while recovery
        // slices were still being written.
        let mut state = started_state(false);
        state.apply(ProgressEvent::Par2EncodeStarted {
            input_bytes: 600_000_000,
            input_slices: 912,
            input_files: 1,
            recovery_slices: 273,
            slice_size: 768_000,
            passes: 1,
            chunk_size: 32_768,
            simd_method: "avx2".to_string(),
            threads: 6,
            memory_limit: 1 << 30,
        });
        state.apply(ProgressEvent::Par2InputProgress {
            done: 912,
            total: 912,
        });
        state.apply(ProgressEvent::Par2WriteStarted { total: 273 });
        // Write all but the last few slices — 1182/1185 ≈ 99.7%.
        for _ in 0..270 {
            state.apply(ProgressEvent::Par2SliceWritten);
        }
        let panel = state.panel_lines(false, 100).join("\n");
        assert!(
            panel.contains("par2  ["),
            "par2 line should still be drawn:\n{panel}"
        );
        assert!(
            !panel.contains("100%"),
            "par2 must not read 100% with slices left:\n{panel}"
        );
    }

    #[test]
    fn status_text_appears_exactly_once() {
        let mut state = started_state(false);
        state.apply(ProgressEvent::Status {
            text: "memory: address-space limit none detected".to_string(),
        });
        let hits = state
            .panel_lines(false, 200)
            .iter()
            .filter(|l| l.contains("address-space limit"))
            .count();
        assert_eq!(hits, 1, "status was drawn twice (cyan line + `▸` line)");
    }

    /// Drive `state` to upload-complete with the streaming check part-way
    /// through — the tail where the old panel showed "posting PAR2" and a
    /// bar-less check box.
    fn upload_done_check_running() -> RenderState {
        let mut state = started_state(false);
        let remaining = state.total_segments - state.done_segments;
        for _ in 0..remaining {
            state.apply(ProgressEvent::SegmentDone {
                file: "Movie.2026/movie.mkv".to_string(),
                bytes: 768_000,
                ok: true,
            });
        }
        // Half the plan confirmed by the check queue.
        state.apply(ProgressEvent::CheckProgress {
            checked: state.total_segments / 2,
            ok: true,
        });
        state
    }

    #[test]
    fn header_says_verifying_once_the_upload_is_done_and_check_runs() {
        let state = upload_done_check_running();
        let header = &state.panel_lines(false, 100)[0];
        assert!(
            header.contains("verifying"),
            "header should move on from posting to verifying: {header:?}"
        );
        assert!(
            !header.contains("posting"),
            "header still claims posting after the upload finished: {header:?}"
        );
    }

    #[test]
    fn check_box_has_no_redundant_bar() {
        // The upload bar already shows check progress as its trailing blue
        // band; a second bar in the check box duplicated it. The box keeps the
        // verified/pending tally but draws no bar, percentage, or `N/M
        // checked` line of its own.
        let state = upload_done_check_running();
        let panel = state.panel_lines(false, 100);
        let tally = panel
            .iter()
            .find(|l| l.contains("verified"))
            .expect("check box tally line drawn");
        assert!(
            !tally.contains('%'),
            "check box should not show its own percentage: {tally:?}"
        );
        assert!(
            !panel.iter().any(|l| l.contains("checked")),
            "the separate check bar line should be gone"
        );
    }

    #[test]
    fn check_is_excluded_from_the_overall_eta() {
        // The check's bursty throughput would distort the ETA, so it must not
        // feed `overall_eta_secs`. With the upload complete and no PAR2 work
        // pending, the ETA resolves to None rather than a check-derived guess.
        let state = upload_done_check_running();
        assert!(
            state.overall_eta_secs().is_none(),
            "a lone draining check must not synthesise an ETA"
        );
    }

    #[test]
    fn connection_activity_is_a_single_dot_row() {
        let mut state = started_state(false);
        state.apply(ProgressEvent::ConnectionBusy {
            conn: 0,
            file: "Movie.2026/movie.mkv".to_string(),
        });
        let panel = state.panel_lines(false, 100);
        let conn_lines: Vec<&String> = panel.iter().filter(|l| l.contains("conns")).collect();
        assert_eq!(conn_lines.len(), 1, "the grid should collapse to one line");
        let line = conn_lines[0];
        assert!(line.contains('●'), "a busy worker should show a filled dot");
        assert!(line.contains('○'), "idle workers should show hollow dots");
        assert!(line.contains("7/7 active") || line.contains("1/7 active"));
    }

    #[test]
    fn final_summary_is_two_lines_and_reports_the_outcome() {
        let mut state = upload_done_check_running();
        // Finish the check clean.
        state.apply(ProgressEvent::CheckProgress {
            checked: state.total_segments,
            ok: true,
        });
        state.apply(ProgressEvent::CheckDone { failed: 0 });
        let summary = state.summary_lines(100);
        assert_eq!(summary.len(), 2, "summary is a compact two lines");
        assert!(summary[0].contains('✓'), "clean run gets a check glyph");
        assert!(
            summary[1].contains("all verified"),
            "a fully-confirmed run should say so: {:?}",
            summary[1]
        );
        // No frozen box borders in the summary.
        assert!(!summary.iter().any(|l| l.contains('│')));
    }

    #[test]
    fn final_summary_flags_failures() {
        let mut state = started_state(false);
        state.apply(ProgressEvent::SegmentDone {
            file: "Movie.2026/movie.mkv".to_string(),
            bytes: 768_000,
            ok: false,
        });
        let summary = state.summary_lines(100);
        assert!(summary[0].contains('✗'), "a failed run gets a cross glyph");
        assert!(summary[1].contains("failed"));
    }

    #[test]
    fn box_borders_align_with_wide_and_emoji_filenames() {
        // A CJK ideograph / wide emoji is two columns; `visible_len` measures
        // display width now, so the padded box interior stays rectangular even
        // when the active-file line below carries such a name.
        let mut state = RenderState::new();
        state.apply(ProgressEvent::Started {
            mode: RunMode::Post,
            files: vec![FileEntry {
                name: "映画作品２０２６/動画🎬.mkv".to_string(),
                segments: 70,
                bytes: 50_000_000,
            }],
            connections: 4,
            check_connections: 1,
            target: Some("news.example.com:563".to_string()),
            par2_bytes_hint: 0,
            par2_segments_hint: 0,
        });
        state.apply(ProgressEvent::ConnectionBusy {
            conn: 0,
            file: "映画作品２０２６/動画🎬.mkv".to_string(),
        });
        state.apply(ProgressEvent::SegmentDone {
            file: "映画作品２０２６/動画🎬.mkv".to_string(),
            bytes: 768_000,
            ok: true,
        });
        for width in [40, 60, 80] {
            let lines = state.panel_lines(false, width);
            let box_lines: Vec<&String> = lines
                .iter()
                .filter(|l| l.starts_with('┌') || l.starts_with('│') || l.starts_with('└'))
                .collect();
            let expected = visible_len(box_lines[0]);
            for line in &box_lines {
                assert_eq!(
                    visible_len(line),
                    expected,
                    "wide-char content skewed a box edge at width={width}: {line:?}"
                );
            }
        }
    }
}
