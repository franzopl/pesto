//! Progress reporting.
//!
//! The poster never writes to the terminal directly. Instead it emits
//! [`ProgressEvent`]s on a channel, which keeps `pesto` usable as a library:
//! an embedding application (e.g. `upapasta`) drains the channel and renders
//! progress however it likes, while the `pesto` binary installs the built-in
//! [`spawn_terminal_renderer`] panel.
//!
//! Events flow over an *unbounded* channel so emitting one never blocks the
//! hot posting path; dropping the receiver simply makes emission a no-op.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

/// Sender half of the progress channel handed to the poster.
pub type ProgressSender = UnboundedSender<ProgressEvent>;
/// Receiver half drained by a renderer or an embedding application.
pub type ProgressReceiver = UnboundedReceiver<ProgressEvent>;

/// What kind of run is producing events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Files are encoded and posted over NNTP.
    Post,
    /// Files are processed but never sent over the network.
    DryRun,
    /// Only PAR2 parity files are generated, written next to the sources.
    Par2Only,
}

/// One file in the run, as announced by [`ProgressEvent::Started`].
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub segments: u64,
    pub bytes: u64,
}

/// An observable step of a posting run.
///
/// The stream always opens with [`Started`](ProgressEvent::Started) and ends
/// with [`Finished`](ProgressEvent::Finished); everything in between is
/// incremental. The channel closing is equivalent to `Finished`.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// The run begins. Carries the full work plan.
    Started {
        mode: RunMode,
        files: Vec<FileEntry>,
        /// Number of NNTP connections / worker threads (0 for `--par2-only`).
        connections: usize,
        /// `host:port` of the NNTP server, or `None` when not posting.
        target: Option<String>,
    },
    /// Worker connection `conn` started posting a segment of `file`.
    ConnectionBusy { conn: usize, file: String },
    /// Worker connection `conn` drained the queue and stopped.
    ConnectionIdle { conn: usize },
    /// One segment of `file` finished; `bytes` is its raw payload size.
    /// `ok` is false when the segment failed every retry.
    SegmentDone { file: String, bytes: u64, ok: bool },
    /// Extra work was appended to the queue — the PAR2 files, which only
    /// exist once the data pass has computed parity.
    QueueExtended {
        file: String,
        segments: u64,
        bytes: u64,
    },
    /// A short human-readable status note (empty string clears it).
    Status { text: String },
    /// A segment failed permanently after exhausting its retries.
    Failed { description: String },
    /// Ctrl-C was received; the run is winding down.
    Interrupted,
    /// Terminal event: the run is over.
    Finished,
    /// Archive compression has started. `total_bytes` is the sum of raw input
    /// sizes — a tight bound for the archive in store mode (no compression).
    CompressStarted { total_bytes: u64 },
    /// Archive file on disk has grown to `bytes_written` bytes (polled ~200 ms).
    CompressProgress { bytes_written: u64 },
    /// Compression finished; the archive file is complete.
    CompressDone,
    /// PAR2 recovery slice writing has started; `total` slices will be written.
    Par2WriteStarted { total: u32 },
    /// One PAR2 recovery slice has been appended to its volume file on disk.
    Par2SliceWritten,
    /// Worker connection `conn` is authenticating (shown in yellow).
    ConnectionAuth { conn: usize },
    /// Worker connection `conn` is retrying a failed segment (shown in red).
    ConnectionRetrying { conn: usize },
    /// Snapshot of the buffer pool: `total` pre-allocated buffers, `free` available.
    BufferPoolStats { total: usize, free: usize },
}

/// Options controlling the built-in terminal renderer.
#[derive(Debug, Clone, Default)]
pub struct RendererOptions {
    /// Show only a single spinning line instead of the full panel.
    pub quiet: bool,
    /// Ring the terminal bell (`\a`) when the run finishes.
    pub bell: bool,
}

/// Spawn the built-in terminal renderer used by the `pesto` binary.
///
/// Returns the [`ProgressSender`] to hand to the poster and the renderer's
/// [`JoinHandle`], which the caller awaits once posting has returned. On a
/// real TTY it draws an in-place multi-line panel; otherwise it prints plain,
/// scroll-friendly status lines suitable for logs and CI.
pub fn spawn_terminal_renderer() -> (ProgressSender, JoinHandle<()>) {
    spawn_terminal_renderer_with(RendererOptions::default())
}

/// Like [`spawn_terminal_renderer`] but with explicit display options.
pub fn spawn_terminal_renderer_with(opts: RendererOptions) -> (ProgressSender, JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(render_loop(rx, opts));
    (tx, handle)
}

/// Spawn a newline-delimited JSON emitter for machine-readable consumers
/// (e.g. `upapasta`).
///
/// Each [`ProgressEvent`] is translated to one JSON object printed to stdout.
/// After the emitter finishes, the caller should print a
/// `{"type":"nzb_written","path":"..."}` event itself once the NZB file has
/// been written. This decouples path resolution from the progress stream.
///
/// Returns the [`ProgressSender`] to hand to the poster and a [`JoinHandle`]
/// the caller must await after posting returns.
pub fn spawn_json_emitter() -> (ProgressSender, JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(json_emit_loop(rx));
    (tx, handle)
}

async fn json_emit_loop(mut rx: ProgressReceiver) {
    let stdout = std::io::stdout();
    let mut total_segments: u64 = 0;
    let mut done_segments: u64 = 0;
    let mut done_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut failures: u64 = 0;

    loop {
        match rx.recv().await {
            None | Some(ProgressEvent::Finished) => {
                let pct = if total_segments > 0 {
                    (done_segments as f64 / total_segments as f64 * 100.0).min(100.0)
                } else {
                    100.0
                };
                let ok = failures == 0;
                let mut out = stdout.lock();
                let _ = writeln!(
                    out,
                    r#"{{"type":"finished","segments":{done_segments},"failures":{failures},"progress_pct":{pct:.1},"ok":{ok}}}"#
                );
                break;
            }
            Some(ev) => {
                let mut out = stdout.lock();
                match ev {
                    ProgressEvent::Started {
                        files,
                        connections,
                        target,
                        ..
                    } => {
                        let target_json = target
                            .as_deref()
                            .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
                            .unwrap_or_else(|| "null".to_string());
                        for f in &files {
                            total_segments += f.segments;
                            total_bytes += f.bytes;
                        }
                        let _ = writeln!(
                            out,
                            r#"{{"type":"started","total_files":{nf},"total_bytes":{total_bytes},"total_segments":{total_segments},"connections":{connections},"target":{target_json}}}"#,
                            nf = files.len(),
                        );
                    }
                    ProgressEvent::SegmentDone { file, bytes, ok } => {
                        done_segments += 1;
                        done_bytes += bytes;
                        if !ok {
                            failures += 1;
                        }
                        let pct = if total_segments > 0 {
                            (done_segments as f64 / total_segments as f64 * 100.0).min(100.0)
                        } else {
                            0.0
                        };
                        let file_esc = file.replace('"', "\\\"");
                        let _ = writeln!(
                            out,
                            r#"{{"type":"segment_done","file":"{file_esc}","bytes":{bytes},"ok":{ok},"done_segments":{done_segments},"total_segments":{total_segments},"done_bytes":{done_bytes},"total_bytes":{total_bytes},"progress_pct":{pct:.1}}}"#
                        );
                    }
                    ProgressEvent::QueueExtended {
                        file,
                        segments,
                        bytes,
                    } => {
                        total_segments += segments;
                        total_bytes += bytes;
                        let file_esc = file.replace('"', "\\\"");
                        let _ = writeln!(
                            out,
                            r#"{{"type":"queue_extended","file":"{file_esc}","segments":{segments},"bytes":{bytes},"total_segments":{total_segments},"total_bytes":{total_bytes}}}"#
                        );
                    }
                    ProgressEvent::Status { text } => {
                        let text_esc = text.replace('\\', "\\\\").replace('"', "\\\"");
                        let _ = writeln!(out, r#"{{"type":"status","text":"{text_esc}"}}"#);
                    }
                    ProgressEvent::Failed { description } => {
                        let desc_esc = description.replace('\\', "\\\\").replace('"', "\\\"");
                        let _ = writeln!(out, r#"{{"type":"failed","description":"{desc_esc}"}}"#);
                    }
                    ProgressEvent::Interrupted => {
                        let _ = writeln!(out, r#"{{"type":"interrupted"}}"#);
                    }
                    ProgressEvent::CompressStarted { total_bytes: tb } => {
                        let _ =
                            writeln!(out, r#"{{"type":"compress_started","total_bytes":{tb}}}"#);
                    }
                    ProgressEvent::CompressProgress { bytes_written } => {
                        let _ = writeln!(
                            out,
                            r#"{{"type":"compress_progress","bytes_written":{bytes_written}}}"#
                        );
                    }
                    ProgressEvent::CompressDone => {
                        let _ = writeln!(out, r#"{{"type":"compress_done"}}"#);
                    }
                    ProgressEvent::Par2WriteStarted { total } => {
                        let _ = writeln!(out, r#"{{"type":"par2_write_started","total":{total}}}"#);
                    }
                    ProgressEvent::Par2SliceWritten => {
                        let _ = writeln!(out, r#"{{"type":"par2_slice_written"}}"#);
                    }
                    // Connection and pool events are noisy and not useful to consumers.
                    ProgressEvent::ConnectionBusy { .. }
                    | ProgressEvent::ConnectionIdle { .. }
                    | ProgressEvent::ConnectionAuth { .. }
                    | ProgressEvent::ConnectionRetrying { .. }
                    | ProgressEvent::BufferPoolStats { .. }
                    | ProgressEvent::Finished => {}
                }
            }
        }
    }
}

/// Width, in characters, of the panel box interior.
const BODY_W: usize = 56;
/// Above this connection count the per-connection grid is replaced by a
/// one-line summary, so the panel never grows unbounded.
const GRID_LIMIT: usize = 12;

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
        }
    }

    fn apply(&mut self, ev: ProgressEvent) {
        match ev {
            ProgressEvent::Started {
                mode,
                files,
                connections,
                target,
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
                self.total_segments += segments;
                self.total_bytes += bytes;
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
        let variance = samples.iter().map(|&x| (x - mean).powi(2)).sum::<f64>()
            / samples.len() as f64;
        let sigma = variance.sqrt();
        let cv = sigma / mean;

        let mid = remaining / mean;
        if cv < 0.1 {
            return Some((mid, mid, false));
        }
        let low = remaining / (mean + sigma).max(1.0);
        let high = remaining / (mean - sigma).max(1.0);
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
        let current_bps = self.done_bytes.saturating_sub(self.prev_done_bytes) as f64
            * (1000.0 / 200.0); // per-tick delta → bytes/s (200 ms tick)
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

        // --- header ------------------------------------------------------
        if self.compress_active && self.files.is_empty() {
            lines.push("pesto · compressing".to_string());
        } else {
            let verb = match self.mode {
                RunMode::Post => "posting",
                RunMode::DryRun => "dry run",
                RunMode::Par2Only => "par2",
            };
            let file_count = self.files.len();
            let mut header = format!("pesto · {verb} {file_count} file(s)");
            if let Some(t) = &self.target {
                header.push_str(&format!(" → {t}"));
            }
            lines.push(header);
        }

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
            let frac = if self.total_bytes > 0 {
                (self.done_bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
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
            // Line 2: bytes transferred + speed + sparkline (phase 21c)
            let spark = {
                let samples = self.speed_samples();
                if samples.len() >= 2 {
                    format!(" {}", render_sparkline(&samples))
                } else {
                    String::new()
                }
            };
            let line2 = format!(
                "{}/{} · {}/s{}",
                format_size(self.done_bytes),
                format_size(self.total_bytes),
                format_size(rate as u64),
                spark,
            );
            // Line 3: ETA with confidence range (phase 21d)
            let line3 = if final_draw {
                format!("elapsed {}", format_duration(self.elapsed_secs()))
            } else if let Some((lo, hi, unstable)) = self.eta_range() {
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
            lines.push(format!("┌─ overall {}┐", "─".repeat(BODY_W + 2 - 10)));
            lines.push(box_line(&line1));
            lines.push(box_line(&line2));
            lines.push(box_line(&line3));
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
                        format!("{left}{}", conn_cell(idx + 1, &self.conn_files[idx + 1], st_r))
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
            let failures_str = if self.failures > 0 {
                ansi(&format!("failures {}", self.failures), "31")
            } else {
                format!("failures {}", self.failures)
            };
            lines.push(format!("files ✓{done} ⤵{in_flight} · {failures_str}"));

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

        // --- PAR2 recovery slice writing progress ------------------------
        if self.par2_write_active || (final_draw && self.par2_write_total > 0) {
            let elapsed = self.par2_write_start.elapsed().as_secs_f64().max(0.001);
            let frac = if self.par2_write_total > 0 {
                (self.par2_write_done as f64 / self.par2_write_total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let bar = render_bar(frac, 10);
            let rate = self.par2_write_done as f64 / elapsed;
            let eta_str = if final_draw {
                format!(" · elapsed {}", format_duration(elapsed))
            } else if rate > 0.1 && self.par2_write_total > self.par2_write_done {
                let remaining = (self.par2_write_total - self.par2_write_done) as f64 / rate;
                format!(" · ETA {}", format_duration(remaining))
            } else {
                String::new()
            };
            lines.push(format!(
                "▸ PAR2 [{bar}] {}/{} slices{eta_str}",
                self.par2_write_done, self.par2_write_total
            ));
        }

        // --- process resource stats (Linux /proc/self) -------------------
        #[cfg(target_os = "linux")]
        if self.proc_rss_bytes > 0 {
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
        ConnState::Busy => ansi(&raw, "32"),      // green
        ConnState::Auth => ansi(&raw, "33"),      // yellow
        ConnState::Retrying => ansi(&raw, "31"),  // red
        ConnState::Idle => ansi(&raw, "2"),       // dim
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

/// Human-readable byte size with binary (IEC) units.
pub fn format_size(bytes: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_uses_binary_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn format_duration_splits_minutes_and_hours() {
        assert_eq!(format_duration(5.0), "0:05");
        assert_eq!(format_duration(125.0), "2:05");
        assert_eq!(format_duration(3661.0), "1:01:01");
    }

    #[test]
    fn render_bar_is_proportional() {
        assert_eq!(render_bar(0.0, 4), "░░░░");
        assert_eq!(render_bar(1.0, 4), "████");
        // 0.5 of 4 cells = 2 full blocks exactly (no fractional remainder)
        assert_eq!(render_bar(0.5, 4), "██░░");
        // Sub-character: 0.125 of 8 cells = 1 eighth → ▏ then 7 empty
        assert_eq!(render_bar(0.125 / 8.0, 8), "▏░░░░░░░");
        // Ensure visual width equals `width` (each char is one column)
        assert_eq!(render_bar(0.3, 10).chars().count(), 10);
    }

    #[test]
    fn render_sparkline_maps_to_nine_levels() {
        let samples = vec![0.0, 50.0, 100.0];
        let spark = render_sparkline(&samples);
        let chars: Vec<char> = spark.chars().collect();
        assert_eq!(chars.len(), 3);
        assert_eq!(chars[0], ' '); // min → space
        assert_eq!(chars[2], '█'); // max → full block
    }

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a-very-long-name", 8), "a-very-…");
    }

    #[test]
    fn box_line_is_exact_width() {
        let line = box_line("hello");
        // `│ ` + BODY_W chars + ` │`
        assert_eq!(line.chars().count(), BODY_W + 4);
    }

    #[test]
    fn file_tally_counts_done_and_in_flight() {
        let mut st = RenderState::new();
        st.files.insert("a".into(), (10, 10));
        st.files.insert("b".into(), (3, 10));
        st.files.insert("c".into(), (0, 10));
        assert_eq!(st.file_tally(), (1, 1));
    }

    #[test]
    fn segment_done_updates_totals_and_failures() {
        let mut st = RenderState::new();
        st.apply(ProgressEvent::Started {
            mode: RunMode::Post,
            files: vec![FileEntry {
                name: "a".into(),
                segments: 2,
                bytes: 100,
            }],
            connections: 1,
            target: None,
        });
        st.apply(ProgressEvent::SegmentDone {
            file: "a".into(),
            bytes: 60,
            ok: true,
        });
        st.apply(ProgressEvent::SegmentDone {
            file: "a".into(),
            bytes: 40,
            ok: false,
        });
        assert_eq!(st.done_segments, 2);
        assert_eq!(st.done_bytes, 100);
        assert_eq!(st.failures, 1);
        assert_eq!(st.file_tally(), (1, 0));
    }
}
