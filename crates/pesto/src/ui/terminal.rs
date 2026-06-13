use crate::progress::{ProgressEvent, ProgressReceiver, ProgressSender, RendererOptions, RunMode};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Width, in characters, of the panel box interior.
const BODY_W: usize = 56;
/// Above this connection count the per-connection grid is replaced by a
/// one-line summary, so the panel never grows unbounded.
const GRID_LIMIT: usize = 12;

/// Spawn the built-in terminal renderer used by the `pesto` binary.
pub fn spawn_renderer() -> (ProgressSender, JoinHandle<()>) {
    spawn_renderer_with(RendererOptions::default())
}

/// Like [`spawn_renderer`] but with explicit display options.
pub fn spawn_renderer_with(opts: RendererOptions) -> (ProgressSender, JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(render_loop(rx, opts));
    (tx, handle)
}

async fn render_loop(mut rx: ProgressReceiver, opts: RendererOptions) {
    let tty = std::io::stderr().is_terminal();
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
                            state.draw_panel(true);
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
    /// Whether any QueueExtended event was received (PAR2 files being posted).
    posting_par2: bool,
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
    // Post-check phase
    check_active: bool,
    check_total: u64,
    check_checked: u64,
    check_failed: u64,
    check_start: Instant,
    check_retry_msg: Option<String>,
    check_waiting_secs: Option<u64>,
    // PAR2 encode info block (shown while encoding, inspired by parpar)
    par2_info: Option<Par2Info>,
    // PAR2 input slice encode progress
    par2_encode_done: usize,
    par2_encode_total: usize,
    par2_encode_start: Instant,
}

#[derive(Debug, Clone)]
struct Par2Info {
    input_bytes: u64,
    input_slices: usize,
    input_files: usize,
    recovery_slices: usize,
    slice_size: usize,
    passes: usize,
    chunk_size: usize,
    simd_method: String,
    threads: usize,
    memory_limit: usize,
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
            check_active: false,
            check_total: 0,
            check_checked: 0,
            check_failed: 0,
            check_start: Instant::now(),
            check_retry_msg: None,
            check_waiting_secs: None,
            par2_info: None,
            par2_encode_done: 0,
            par2_encode_total: 0,
            par2_encode_start: Instant::now(),
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
            posting_par2: false,
        }
    }

    fn apply(&mut self, ev: ProgressEvent) {
        match ev {
            ProgressEvent::Started {
                mode,
                files,
                connections,
                target,
                par2_bytes_hint,
            } => {
                self.started = true;
                self.mode = mode;
                self.target = target;
                self.start = Instant::now();
                self.conn_files = vec![None; connections];
                self.conn_state = vec![ConnState::Idle; connections];
                for f in files {
                    self.total_segments += f.segments;
                    self.total_bytes += f.bytes;
                    self.files.insert(f.name, (0, f.segments));
                }
                // Pre-seed total with the PAR2 estimate so the bar never
                // goes backwards when QueueExtended arrives.
                self.total_bytes += par2_bytes_hint;
                self.par2_hint_remaining = par2_bytes_hint;
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
                self.total_segments += segments;
                // Absorb the real bytes against the pre-seeded hint so the bar
                // does not jump backwards. If real PAR2 is larger than the hint,
                // we expand total_bytes by the excess only.
                if bytes <= self.par2_hint_remaining {
                    self.par2_hint_remaining -= bytes;
                    // total_bytes already includes this; no change needed.
                } else {
                    let excess = bytes - self.par2_hint_remaining;
                    self.par2_hint_remaining = 0;
                    self.total_bytes += excess;
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
            ProgressEvent::Par2EncodeStarted {
                input_bytes,
                input_slices,
                input_files,
                recovery_slices,
                slice_size,
                passes,
                chunk_size,
                simd_method,
                threads,
                memory_limit,
            } => {
                self.par2_encode_total = input_slices;
                self.par2_encode_done = 0;
                self.par2_encode_start = Instant::now();
                self.par2_info = Some(Par2Info {
                    input_bytes,
                    input_slices,
                    input_files,
                    recovery_slices,
                    slice_size,
                    passes,
                    chunk_size,
                    simd_method,
                    threads,
                    memory_limit,
                });
            }
            ProgressEvent::Par2InputProgress { done, total } => {
                self.par2_encode_done = done;
                self.par2_encode_total = total;
            }
            ProgressEvent::Par2WriteStarted { total } => {
                self.par2_write_active = true;
                self.par2_write_total = total;
                self.par2_write_done = 0;
                self.par2_write_start = Instant::now();
            }
            ProgressEvent::Par2SliceWritten => {
                self.par2_write_done = self.par2_write_done.saturating_add(1);
                if self.par2_write_done >= self.par2_write_total {
                    self.par2_write_active = false;
                }
            }
            ProgressEvent::CheckStarted { total } => {
                self.started = true; // allow draw_panel when used as a standalone check renderer
                self.check_active = true;
                self.check_total = total;
                self.check_checked = 0;
                self.check_failed = 0;
                self.check_start = Instant::now();
                self.check_retry_msg = None;
                self.check_waiting_secs = None;
            }
            ProgressEvent::CheckWaiting { remaining_secs } => {
                self.started = true;
                self.check_active = true;
                self.check_waiting_secs = Some(remaining_secs);
            }
            ProgressEvent::CheckProgress { checked, ok } => {
                self.check_checked = checked;
                self.check_retry_msg = None;
                self.check_waiting_secs = None;
                if !ok {
                    self.check_failed += 1;
                }
            }
            ProgressEvent::CheckRetrying {
                attempt,
                max_attempts,
                delay_secs,
            } => {
                self.check_retry_msg = Some(format!(
                    "⏳ article not found — retry {attempt}/{max_attempts} in {delay_secs}s",
                ));
            }
            ProgressEvent::CheckDone { failed } => {
                self.check_active = false;
                self.check_failed = failed;
                self.check_retry_msg = None;
                self.check_waiting_secs = None;
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

        let pct = if self.total_bytes > 0 {
            (self.done_bytes as f64 / self.total_bytes as f64 * 100.0).clamp(0.0, 100.0) as u64
        } else {
            0
        };

        let eta_str = if final_draw {
            format!("done {}", format_duration(self.elapsed_secs()))
        } else if let Some((lo, hi, unstable)) = self.eta_range() {
            let mark = if unstable { "~" } else { "" };
            if (hi - lo).abs() < 1.0 {
                format!("ETA {mark}{}", format_duration(lo))
            } else {
                format!("ETA {mark}{}–{}", format_duration(lo), format_duration(hi))
            }
        } else {
            "ETA —".to_string()
        };

        let line = format!("{spinner}  {pct:>3}% · {eta_str}");

        let mut out = String::new();
        if self.lines_drawn > 0 {
            out.push_str("\x1b[1F\x1b[2K");
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
            self.poll_proc_stats();
        }
        let lines = self.panel_lines(final_draw);

        let mut out = String::new();
        // Move the cursor back to the top of the previous panel and wipe
        // everything below it, so a shorter panel leaves no stale lines.
        if self.lines_drawn > 0 {
            out.push_str(&format!("\x1b[{}F", self.lines_drawn));
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

        // --- header with phase indicator and elapsed time ----------------
        let elapsed_hdr = if self.started {
            format!(" · {}", format_duration(self.elapsed_secs()))
        } else {
            String::new()
        };
        let phase = if self.compress_active && self.files.is_empty() {
            ansi("compressing", "33") // yellow
        } else if self.par2_write_active {
            ansi("writing PAR2", "36") // cyan
        } else if self.posting_par2 && !self.files.is_empty() {
            ansi("posting PAR2", "35") // magenta
        } else if !self.files.is_empty() {
            ansi("posting data", "32") // green
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
            let bar = render_bar(frac, 18);
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
            // "─ compressing " = 14 chars (excluding ┌)
            lines.push(format!("┌─ compressing {}┐", "─".repeat(BODY_W + 2 - 14)));
            lines.push(box_line(&line1));
            lines.push(box_line(&line2));
            lines.push(format!("└{}┘", "─".repeat(BODY_W + 2)));
        }

        // --- overall posting box (only after posting has started) --------
        if !self.files.is_empty() {
            // Use segment ratio for the bar percentage so it stays in sync with
            // the N/N seg counter.  total_bytes is pre-seeded with an estimated
            // PAR2 size, so a byte-ratio bar could stop at ~99% while segments
            // already read N/N.  Bytes remain the basis for speed/ETA/size.
            let frac = if self.total_segments > 0 {
                (self.done_segments as f64 / self.total_segments as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let pct = (frac * 100.0).round() as u64;
            let bar = render_bar(frac, 26);
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
                let l3 = if let Some((lo, hi, unstable)) = self.eta_range() {
                    let mark = if unstable { "~" } else { "" };
                    if (hi - lo).abs() < 1.0 {
                        format!("ETA {mark}{}", format_duration(lo))
                    } else {
                        format!("ETA {mark}{}–{}", format_duration(lo), format_duration(hi))
                    }
                } else if rate > 1.0 && self.total_bytes > self.done_bytes {
                    let remaining = (self.total_bytes - self.done_bytes) as f64 / rate;
                    format!("ETA {}", format_duration(remaining))
                } else {
                    "ETA —".to_string()
                };
                (l2, Some(l3))
            };
            lines.push(format!("┌─ upload {}┐", "─".repeat(BODY_W + 2 - 9)));
            lines.push(box_line(&line1));
            lines.push(box_line(&line2));
            if let Some(l3) = line3 {
                lines.push(box_line(&l3));
            }
            lines.push(format!("└{}┘", "─".repeat(BODY_W + 2)));

            // --- per-connection activity with colour codes (phase 21b) ----
            let conns = self.conn_files.len();
            if conns > 0 && conns <= GRID_LIMIT {
                let mut idx = 0;
                while idx < conns {
                    let st_l = self.conn_state.get(idx).copied().unwrap_or_default();
                    let left = conn_cell(idx, &self.conn_files[idx], st_l);
                    let line = if idx + 1 < conns {
                        let st_r = self.conn_state.get(idx + 1).copied().unwrap_or_default();
                        format!(
                            "{left}{}",
                            conn_cell(idx + 1, &self.conn_files[idx + 1], st_r)
                        )
                    } else {
                        left
                    };
                    lines.push(line);
                    idx += 2;
                }
            } else if conns > GRID_LIMIT {
                let active = self.conn_files.iter().filter(|c| c.is_some()).count();
                let retrying = self
                    .conn_state
                    .iter()
                    .filter(|&&s| s == ConnState::Retrying)
                    .count();
                let retry_str = if retrying > 0 {
                    format!(" · {} retrying", ansi(&retrying.to_string(), "31"))
                } else {
                    String::new()
                };
                lines.push(format!("{conns} connections · {active} active{retry_str}"));
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

        // --- PAR2 secondary indicators (encode + write + info), all dim/indented ---
        // Shown below the upload box so the upload bar stays the focal point.
        if !final_draw
            && self.par2_encode_total > 0
            && self.par2_encode_done < self.par2_encode_total
        {
            let elapsed = self.par2_encode_start.elapsed().as_secs_f64().max(0.001);
            let frac =
                (self.par2_encode_done as f64 / self.par2_encode_total as f64).clamp(0.0, 1.0);
            let pct = (frac * 100.0).round() as u64;
            let bar = render_bar(frac, 22);
            let rate = self.par2_encode_done as f64 / elapsed;
            let eta_str = if rate > 0.01 && self.par2_encode_done < self.par2_encode_total {
                let remaining = (self.par2_encode_total - self.par2_encode_done) as f64 / rate;
                format!(" · ETA {}", format_duration(remaining))
            } else {
                String::new()
            };
            lines.push(ansi(
                &format!(
                    "  par2 encode  [{bar}] {pct:>3}%  {}/{} slices{eta_str}",
                    self.par2_encode_done, self.par2_encode_total,
                ),
                "2",
            ));
        }

        if !final_draw && self.par2_write_active {
            let elapsed = self.par2_write_start.elapsed().as_secs_f64().max(0.001);
            let frac = if self.par2_write_total > 0 {
                (self.par2_write_done as f64 / self.par2_write_total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let bar = render_bar(frac, 10);
            let rate = self.par2_write_done as f64 / elapsed;
            let eta_str = if rate > 0.1 && self.par2_write_total > self.par2_write_done {
                let remaining = (self.par2_write_total - self.par2_write_done) as f64 / rate;
                format!(" · ETA {}", format_duration(remaining))
            } else {
                String::new()
            };
            lines.push(ansi(
                &format!(
                    "  par2 write   [{bar}] {}/{} slices{eta_str}",
                    self.par2_write_done, self.par2_write_total
                ),
                "2",
            ));
        }

        if !final_draw && self.par2_encode_done < self.par2_encode_total {
            if let Some(ref info) = self.par2_info {
                let input_str = format!(
                    "{} ({} slice{} from {} file{})",
                    format_size(info.input_bytes),
                    info.input_slices,
                    if info.input_slices == 1 { "" } else { "s" },
                    info.input_files,
                    if info.input_files == 1 { "" } else { "s" },
                );
                let recovery_total = info.recovery_slices as u64 * info.slice_size as u64;
                let recovery_str = format!(
                    "{} ({} × {} slices)",
                    format_size(recovery_total),
                    info.recovery_slices,
                    format_size(info.slice_size as u64),
                );
                let passes_str = format!(
                    "{}, processing {} × {} chunks per pass",
                    info.passes,
                    info.recovery_slices,
                    format_size(info.chunk_size as u64),
                );
                let mem_str = format_size(info.memory_limit as u64);
                lines.push(ansi("  PAR2 encoder", "2"));
                lines.push(ansi(&format!("    Input data      : {input_str}"), "2"));
                lines.push(ansi(&format!("    Recovery data   : {recovery_str}"), "2"));
                lines.push(ansi(&format!("    Input pass(es)  : {passes_str}"), "2"));
                lines.push(ansi(
                    &format!(
                        "    Multiply method : {} · {} threads",
                        info.simd_method, info.threads
                    ),
                    "2",
                ));
                lines.push(ansi(&format!("    Memory usage    : {mem_str}"), "2"));
            }
        }

        // --- persistent status line (e.g. PAR2 details) ------------------
        if !self.status.is_empty() {
            lines.push(ansi(&self.status, "36")); // cyan
        }

        // --- process resource stats (Linux /proc/self) -------------------
        #[cfg(target_os = "linux")]
        if self.proc_rss_bytes > 0 {
            let rss = format_size(self.proc_rss_bytes);
            let cpu = format!("{:.1}%", self.proc_cpu_pct);
            let res_line = format!("process  ram {}  cpu {}", rss, cpu);
            lines.push(ansi(&res_line, "2")); // dim — informational, not critical
        }

        // --- post-check STAT phase ---------------------------------------
        if self.check_active || (final_draw && self.check_total > 0) {
            let elapsed = self.check_start.elapsed().as_secs_f64().max(0.001);

            if let Some(remaining) = self.check_waiting_secs {
                // Propagation delay: show a pulsing wait bar and countdown.
                let line = format!("▸ check  waiting for propagation · {remaining}s remaining");
                lines.push(ansi(&line, "2")); // dim while waiting
            } else {
                let frac = if self.check_total > 0 {
                    (self.check_checked as f64 / self.check_total as f64).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let bar = render_bar(frac, 26);
                let elapsed_str = if !self.check_active {
                    format!(" · elapsed {}", format_duration(elapsed))
                } else {
                    String::new()
                };
                let line = if self.check_failed > 0 {
                    let line = format!(
                        "▸ check  [{}] {}/{} · {} missing{}",
                        bar, self.check_checked, self.check_total, self.check_failed, elapsed_str
                    );
                    ansi(&line, "31") // red — articles missing
                } else if !self.check_active && self.check_total > 0 {
                    let line = format!(
                        "▸ check  [{}] {}/{} · all verified{}",
                        bar, self.check_checked, self.check_total, elapsed_str
                    );
                    ansi(&line, "32") // green — all good
                } else {
                    format!(
                        "▸ check  [{}] {}/{}{}",
                        bar, self.check_checked, self.check_total, elapsed_str
                    )
                };
                lines.push(line);
                if let Some(msg) = &self.check_retry_msg {
                    lines.push(ansi(&format!("  {msg}"), "33")); // yellow retry notice
                }
            }
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
        // Throttle to roughly one line every ~2s so logs stay readable.
        self.plain_ticks += 1;
        if !final_draw && !self.plain_ticks.is_multiple_of(10) {
            return;
        }
        let mut err = std::io::stderr().lock();

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

        if self.check_active {
            let _ = writeln!(
                err,
                "check: {}/{} articles · {} missing",
                self.check_checked, self.check_total, self.check_failed,
            );
            let _ = err.flush();
            return;
        }

        if self.files.is_empty() {
            return;
        }

        let rate = self.rate();
        if final_draw {
            let _ = writeln!(
                err,
                "done: {}/{} segments · {} · {} failures · {}",
                self.done_segments,
                self.total_segments,
                format_size(self.done_bytes),
                self.failures,
                format_duration(self.elapsed_secs()),
            );
        } else {
            let _ = writeln!(
                err,
                "posting: {}/{} segments · {} · {}/s",
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
fn conn_cell(idx: usize, file: &Option<String>, state: ConnState) -> String {
    let label = match file {
        Some(name) => truncate(name, 14),
        None => "idle".to_string(),
    };
    let raw = format!("conn {:<2} ▸ {label}", idx + 1);
    let coloured = match state {
        ConnState::Busy => ansi(&raw, "32"),     // green
        ConnState::Auth => ansi(&raw, "33"),     // yellow
        ConnState::Retrying => ansi(&raw, "31"), // red
        ConnState::Idle => ansi(&raw, "2"),      // dim
    };
    // pad by visual width (raw chars, not ANSI-escaped)
    let visual_len = raw.chars().count();
    let padding = if visual_len < 26 {
        " ".repeat(26 - visual_len)
    } else {
        String::new()
    };
    format!("{coloured}{padding}")
}

/// Format a `│ … │` box content line, padding/truncating to the interior.
fn box_line(body: &str) -> String {
    format!("│ {} │", pad(body, BODY_W))
}

// Eight sub-character blocks from narrowest to fullest (phase 21a).
const SUBCHAR: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Draw a smooth proportional bar using sub-character block rendering.
///
/// The fractional leading cell uses one of `▏▎▍▌▋▊▉█` so the bar moves
/// continuously instead of jumping whole-cell steps.
fn render_bar(frac: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let total_eighths = (frac.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let full_blocks = (total_eighths / 8).min(width);
    let remainder = total_eighths % 8;

    let mut s = String::with_capacity(width * 3); // UTF-8 multi-byte
    for _ in 0..full_blocks {
        s.push('█');
    }
    if full_blocks < width {
        if remainder > 0 {
            s.push(SUBCHAR[remainder - 1]);
            for _ in 0..width - full_blocks - 1 {
                s.push('░');
            }
        } else {
            for _ in 0..width - full_blocks {
                s.push('░');
            }
        }
    }
    s
}

// Nine-level sparkline characters (phase 21c).
const SPARK: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Render a sparkline string from a slice of f64 speed samples.
fn render_sparkline(samples: &[f64]) -> String {
    if samples.is_empty() {
        return String::new();
    }
    let max = samples.iter().cloned().fold(0.0_f64, f64::max);
    if max < 1.0 {
        return SPARK[0].to_string().repeat(samples.len());
    }
    samples
        .iter()
        .map(|&v| {
            let idx = ((v / max) * 8.0).round() as usize;
            SPARK[idx.min(8)]
        })
        .collect()
}

/// Returns the terminal column count queried via `TIOCGWINSZ`, or `None` on
/// non-TTY fds or if the query fails.
#[cfg(unix)]
fn terminal_width() -> Option<usize> {
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    extern "C" {
        fn ioctl(
            fd: std::ffi::c_int,
            request: std::ffi::c_ulong,
            out: *mut Winsize,
        ) -> std::ffi::c_int;
    }

    // TIOCGWINSZ: Linux = 0x5413, macOS = 0x40087468
    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;

    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Safety: ioctl(TIOCGWINSZ) writes exactly sizeof(Winsize) bytes into `ws`.
    let ret = unsafe { ioctl(std::io::stderr().as_raw_fd(), TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn terminal_width() -> Option<usize> {
    None
}

/// Returns true when ANSI colour should be used (TTY + NO_COLOR not set).
fn use_color() -> bool {
    std::io::stderr().is_terminal() && std::env::var("NO_COLOR").is_err()
}

/// Wrap `s` in the given ANSI SGR codes, or return `s` unchanged when colours
/// are disabled.
fn ansi(s: &str, code: &str) -> String {
    if use_color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Pad `s` with spaces (or truncate it) to exactly `width` characters.
fn pad(s: &str, width: usize) -> String {
    let s = truncate(s, width);
    let len = s.chars().count();
    let mut out = String::with_capacity(width);
    out.push_str(&s);
    for _ in 0..width - len {
        out.push(' ');
    }
    out
}

/// Truncate `s` to at most `width` characters, marking a cut with `…`.
fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Format a duration in seconds as `m:ss` (or `h:mm:ss` past an hour).
fn format_duration(secs: f64) -> String {
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
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
