use crate::progress::{ProgressEvent, ProgressReceiver, ProgressSender, RendererOptions, RunMode};
#[cfg(test)]
use crate::ui::format::MIN_BODY_W;
use crate::ui::format::{
    bar_width, body_width, fast_repost_label, format_size, inconclusive_label, render_dual_bar,
    strip_ansi_for_plain, wrapped_note, CHECK_BAND_COLOR, UPLOAD_BAND_COLOR,
};
use crate::ui::render::{
    ansi, box_bottom, box_line, box_top, format_duration, render_bar, render_sparkline,
    terminal_width, truncate, visible_len, wrap,
};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Above this connection count the per-connection grid is replaced by a
/// one-line summary, so the panel never grows unbounded.
const GRID_LIMIT: usize = 12;

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
    /// POST failures still eligible for the automatic end-of-run rescue pass.
    post_retry_pending: u64,
    /// Articles accepted after a transient POST failure.
    recovered_post_retries: u64,
    /// Verification misses whose repost was later confirmed available.
    recovered_check_retries: u64,
    /// Number of typed retry-queue signals awaiting their detailed `Failed` event.
    post_retry_queued_events: u64,
    interrupted: bool,
    aborted: bool,
    /// Set by `ProgressEvent::Failed` (e.g. a producer error such as the
    /// `--memory-limit` address-space check bailing). Printed alongside the
    /// "interrupted" note so a run that dies before posting anything doesn't
    /// leave the user with no clue why.
    failed_description: Option<String>,
    status: String,
    /// Persistent SOCKS5 route indicator, independent of transient status notes.
    proxy_status: Option<String>,
    /// When the current non-empty status text was first set.
    status_since: Option<Instant>,
    /// Last `status` text printed by `draw_plain` — lets it print each new
    /// status line exactly once instead of never (every phase branch
    /// `return`s before reaching a generic status print) or every tick.
    plain_status_printed: String,
    /// Whether `failed_description` has already been printed in plain mode —
    /// mirrors `plain_status_printed`'s one-shot pattern.
    plain_failed_printed: bool,
    /// Whether the one-time "connections: N upload · M check" line has
    /// already been printed in plain (non-TTY) mode.
    plain_connections_printed: bool,
    /// Number of NNTP connections dedicated to the streaming STAT check,
    /// separate from `conn_files`/`conn_state` (upload connections only).
    /// 0 when checking is disabled. Grows past the `Started`-announced
    /// value on `CheckPoolScaledUp` (see `check::CheckCoordinatorHandle::
    /// scale_up`), so this is always the pool's *current* size.
    check_connections: usize,
    /// Whether independent availability checks were enabled for this run.
    checks_enabled: bool,
    /// Busy/idle state of each check-pool connection, indexed the same way
    /// as `conn_state` but in the check pool's own numbering — a connection
    /// is `Busy` only while actually doing a STAT or repost, `Idle`
    /// otherwise (polling an empty queue, or every ready item still inside
    /// its retry/repost delay). Without this the `conns` line only ever
    /// showed the check pool's *configured* size, with no way to tell a
    /// pool working through a backlog from one sitting on backoffs.
    check_conn_state: Vec<ConnState>,
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
    /// True once `Par2PassStarted` supplies an authoritative pass index.
    par2_explicit_pass: bool,
    /// Source reading has finished for the current pass and recovery math is running.
    par2_compute_active: bool,
    /// Recovery packets from the current pass are being flushed to volumes.
    par2_writing_active: bool,
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
    /// Articles whose STAT path failed (transport/timeout/unexpected code)
    /// rather than a confirmed 430. Distinct from `check_failed`.
    check_inconclusive: u64,
    check_reposted: u64,
    check_start: Instant,
    /// Latest fast-repost heuristic snapshot, shown until the run ends.
    check_fast_repost: Option<(u64, u64)>,
    /// Most recent retry backoff still in its window: (label without the
    /// countdown, e.g. "connection error — retry 1/3", deadline). Cleared
    /// once the deadline passes (`expire_check_retry`) rather than on every
    /// resolved article, so a fast-moving pool of concurrent check workers
    /// doesn't wipe it before the user can read it.
    check_retry: Option<(String, Instant)>,
    // Final automatic recovery pass (`poster::check::recover_missing`) — runs
    // strictly *after* the streaming check queue has fully drained
    // (`check_active` is already false by the time this starts), for a
    // small, bounded tail of articles that never got confirmed. It has its
    // own phase/box rather than reusing the check box: it is sequential
    // (one article at a time), so without a dedicated indicator the panel
    // showed a 100% upload bar, an idle connection grid, and nothing else
    // moving for however long the batch took.
    recover_active: bool,
    recover_done: u64,
    recover_total: u64,
    /// Resolutions in this batch that came back `ok: false` (repost failed,
    /// or the final STAT still couldn't confirm it).
    recover_failed: u64,
    recover_start: Instant,
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
            post_retry_pending: 0,
            recovered_post_retries: 0,
            recovered_check_retries: 0,
            post_retry_queued_events: 0,
            interrupted: false,
            aborted: false,
            failed_description: None,
            status: String::new(),
            proxy_status: None,
            status_since: None,
            plain_status_printed: String::new(),
            plain_failed_printed: false,
            plain_connections_printed: false,
            check_connections: 0,
            checks_enabled: false,
            check_conn_state: Vec::new(),
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
            par2_explicit_pass: false,
            par2_compute_active: false,
            par2_writing_active: false,
            check_active: false,
            check_checked: 0,
            check_failed: 0,
            check_inconclusive: 0,
            check_reposted: 0,
            check_start: Instant::now(),
            check_retry: None,
            check_fast_repost: None,
            recover_active: false,
            recover_done: 0,
            recover_total: 0,
            recover_failed: 0,
            recover_start: Instant::now(),
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
                self.checks_enabled = check_connections > 0;
                self.conn_files = vec![None; connections];
                self.conn_state = vec![ConnState::Idle; connections];
                self.check_conn_state = vec![ConnState::Idle; check_connections];
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
                    self.post_retry_pending += 1;
                    // Legacy/manual streams may omit PostRetryQueued; clear
                    // the just-received segment detail once the typed
                    // SegmentDone outcome proves it is rescue-eligible.
                    self.failed_description.take();
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
                let display_text = if text.starts_with("retry:") {
                    "Temporarily retried an article; continuing upload".to_string()
                } else if text.starts_with("check: reposted ") {
                    "Temporarily retried an article; confirming availability".to_string()
                } else {
                    text
                };
                if display_text.is_empty() {
                    self.status_since = None;
                } else if self.status.is_empty() || self.status != display_text {
                    self.status_since = Some(Instant::now());
                }
                self.status = display_text;
            }
            ProgressEvent::ProxyStatus { text } => {
                self.proxy_status = Some(text);
            }
            ProgressEvent::PostRetryQueued => {
                self.post_retry_queued_events += 1;
            }
            ProgressEvent::Failed { description } => {
                if self.post_retry_queued_events > 0 {
                    self.post_retry_queued_events -= 1;
                } else {
                    self.failed_description = Some(description);
                }
            }
            ProgressEvent::PostRetryRecovered {
                count,
                previously_failed,
            } => {
                self.recovered_post_retries += count;
                if previously_failed {
                    self.failures = self.failures.saturating_sub(count);
                    self.post_retry_pending = self.post_retry_pending.saturating_sub(count);
                }
            }
            ProgressEvent::Interrupted => self.interrupted = true,
            ProgressEvent::Aborted => {
                self.interrupted = true;
                self.aborted = true;
            }
            // No CLI flag drives external_pause yet (embedder-only, see
            // ROADMAP.new.md Phase 2) — reuse the existing status line so a
            // future embedder-triggered pause still renders sensibly here.
            ProgressEvent::Paused => {
                self.status = "paused".to_string();
                self.status_since = Some(Instant::now());
            }
            ProgressEvent::Resumed => {
                self.status = String::new();
                self.status_since = None;
            }
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
                self.par2_explicit_pass = false;
                self.par2_compute_active = false;
                self.par2_writing_active = false;
                self.started = true;
            }
            ProgressEvent::Par2PassStarted { pass, passes } => {
                self.par2_passes = passes.max(1);
                self.par2_pass_index = pass.saturating_sub(1).min(self.par2_passes - 1);
                self.par2_encode_done = 0;
                self.par2_explicit_pass = true;
                self.par2_compute_active = false;
                self.par2_writing_active = false;
                self.started = true;
            }
            ProgressEvent::Par2ComputeStarted { pass, passes } => {
                self.par2_passes = passes.max(1);
                self.par2_pass_index = pass.saturating_sub(1).min(self.par2_passes - 1);
                self.par2_compute_active = true;
                self.par2_writing_active = false;
                self.started = true;
            }
            ProgressEvent::Par2InputProgress { done, total } => {
                // Keep the legacy rollover inference for embedding callers
                // that do not emit the additive Par2PassStarted signal.
                if !self.par2_explicit_pass && done < self.par2_encode_done {
                    let next = (self.par2_pass_index + 1).min(self.par2_passes.saturating_sub(1));
                    if next > self.par2_pass_index {
                        self.par2_pass_index = next;
                        self.par2_encode_done = done;
                    }
                } else if self.par2_explicit_pass {
                    self.par2_encode_done = self.par2_encode_done.max(done);
                } else {
                    self.par2_encode_done = done;
                }
                self.par2_compute_active = false;
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
                self.par2_compute_active = false;
                self.par2_writing_active = true;
                self.par2_write_done = self.par2_write_done.saturating_add(1);
                if self.par2_write_done >= self.par2_write_total {
                    self.par2_write_active = false;
                    self.par2_writing_active = false;
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
            ProgressEvent::CheckInconclusive { count, .. } => {
                if !self.check_active {
                    self.started = true;
                    self.check_active = true;
                    self.check_start = Instant::now();
                }
                if count > self.check_inconclusive {
                    self.check_checked = self
                        .check_checked
                        .saturating_add(count - self.check_inconclusive);
                }
                self.check_inconclusive = count;
            }
            ProgressEvent::CheckFastRepost {
                first_checks,
                first_misses,
            } => {
                if !self.check_active {
                    self.started = true;
                    self.check_active = true;
                    self.check_start = Instant::now();
                }
                self.check_fast_repost = Some((first_checks, first_misses));
            }
            ProgressEvent::CheckRetrying {
                attempt,
                max_attempts,
                delay_secs,
                reason: _,
            } => {
                self.check_retry = Some((
                    format!("temporarily retrying availability check ({attempt}/{max_attempts})"),
                    Instant::now() + Duration::from_secs(delay_secs),
                ));
            }
            ProgressEvent::CheckReposted { reposted } => {
                self.check_reposted = reposted;
            }
            ProgressEvent::CheckRetryRecovered => {
                self.recovered_check_retries += 1;
            }
            ProgressEvent::CheckDone {
                failed,
                inconclusive,
            } => {
                self.check_active = false;
                self.check_failed = failed;
                if inconclusive > self.check_inconclusive {
                    self.check_checked = self
                        .check_checked
                        .saturating_add(inconclusive - self.check_inconclusive);
                }
                self.check_inconclusive = inconclusive;
                self.check_retry = None;
            }
            ProgressEvent::CheckRecoverStarted { total } => {
                self.recover_active = true;
                self.recover_done = 0;
                self.recover_total = total;
                self.recover_failed = 0;
                self.recover_start = Instant::now();
            }
            ProgressEvent::CheckRecoverProgress { done, total, ok } => {
                self.recover_done = done;
                self.recover_total = total;
                if ok {
                    // This article was part of `check_failed`'s count from
                    // `CheckDone` (that's how it ended up in this batch);
                    // now that it's confirmed, the "missing" tally and the
                    // final summary line must reflect that instead of
                    // freezing on the pre-recovery number. Also counts
                    // toward `check_reposted` — the streaming coordinator's
                    // own counter never sees recovery-pass reposts, since
                    // `recover_missing` runs standalone, after that
                    // coordinator has already shut down.
                    self.check_failed = self.check_failed.saturating_sub(1);
                    self.check_reposted += 1;
                    self.recovered_check_retries += 1;
                } else {
                    self.recover_failed += 1;
                }
                if done >= total {
                    self.recover_active = false;
                }
            }
            ProgressEvent::CheckConnectionBusy { conn } => {
                if let Some(s) = self.check_conn_state.get_mut(conn) {
                    *s = ConnState::Busy;
                }
            }
            ProgressEvent::CheckConnectionIdle { conn } => {
                if let Some(s) = self.check_conn_state.get_mut(conn) {
                    *s = ConnState::Idle;
                }
            }
            ProgressEvent::CheckPoolScaledUp { check_connections } => {
                self.check_connections = check_connections;
                self.check_conn_state
                    .resize(check_connections, ConnState::Idle);
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

    fn recovered_retries(&self) -> u64 {
        self.recovered_post_retries + self.recovered_check_retries
    }

    fn confirmed_articles(&self) -> u64 {
        self.check_checked
            .saturating_sub(self.check_failed)
            .saturating_sub(self.check_inconclusive)
    }

    fn accepted_articles(&self) -> u64 {
        self.done_segments.saturating_sub(self.failures)
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
            // Reserve the red-style cross for unresolved upload/availability
            // failures. Cancellation remains a warning rather than looking
            // like a server rejected data.
            if self.failures > 0 || self.check_failed > 0 || self.check_inconclusive > 0 {
                '✗'
            } else if self.interrupted || self.failed_description.is_some() {
                '⚠'
            } else {
                '✓'
            }
        } else {
            let ch = SPINNER[self.spinner_frame % SPINNER.len()];
            self.spinner_frame += 1;
            ch
        };

        let pct = (self.progress_frac() * 100.0).round() as u64;

        let eta_str = if final_draw {
            let recovered = self.recovered_retries();
            let recovered_note = if recovered > 0 {
                format!(
                    " · {recovered} {} recovered automatically",
                    if recovered == 1 { "retry" } else { "retries" }
                )
            } else {
                String::new()
            };
            if self.failures + self.check_failed > 0 {
                let inconclusive_note = if self.check_inconclusive > 0 {
                    format!(" · {}", inconclusive_label(self.check_inconclusive))
                } else {
                    String::new()
                };
                format!(
                    "upload incomplete · {} unresolved{inconclusive_note}",
                    self.failures + self.check_failed
                )
            } else if self.mode == RunMode::Post && self.checks_enabled {
                format!(
                    "complete · {}/{} confirmed{recovered_note}",
                    self.confirmed_articles(),
                    self.total_segments
                )
            } else if self.mode == RunMode::Post {
                format!(
                    "complete · {}/{} accepted · not verified{recovered_note}",
                    self.accepted_articles(),
                    self.total_segments
                )
            } else {
                format!("done {}", format_duration(self.elapsed_secs()))
            }
        } else if self.recover_active {
            // `pct` is already 100% here (it tracks upload segments, and the
            // recovery pass only starts once every one of those is done),
            // so without this the quiet line reads "100% · ETA —" for
            // however long the sequential recovery batch takes — visually
            // indistinguishable from a hang.
            format!(
                "recovering availability {}/{}",
                self.recover_done, self.recover_total
            )
        } else if self.par2_compute_active {
            format!(
                "computing recovery data · pass {}/{}",
                self.par2_pass_index + 1,
                self.par2_passes
            )
        } else if self.par2_writing_active {
            format!(
                "writing recovery volumes · pass {}/{}",
                self.par2_pass_index + 1,
                self.par2_passes
            )
        } else if self.par2_encode_units_done() < self.par2_encode_units_total() {
            format!(
                "reading sources · pass {}/{}",
                self.par2_pass_index + 1,
                self.par2_passes
            )
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

    /// The compact run summary that replaces the live panel once the run is
    /// over. A recovered retry gets its own optional third line so the primary
    /// success message stays readable at ordinary terminal widths. The binary
    /// follows it with the `wrote
    /// nzb`/`wrote nfo` paths, so this covers only what the renderer itself
    /// knows — outcome, throughput and verification.
    fn summary_lines(&self, width: usize) -> Vec<String> {
        let unresolved = self.failures + self.check_failed + self.check_inconclusive;
        let ok = unresolved == 0 && !self.interrupted && self.failed_description.is_none();
        let glyph = if self.interrupted || self.failed_description.is_some() {
            ansi("⚠", "33")
        } else if ok {
            ansi("✓", "32")
        } else {
            ansi("✗", "31")
        };

        let mut lines = match self.mode {
            RunMode::Post if self.interrupted || self.failed_description.is_some() => vec![
                format!("{glyph} Upload stopped before completion."),
                format!(
                    "  {}/{} articles accepted · elapsed {}",
                    self.accepted_articles(),
                    self.total_segments,
                    format_duration(self.elapsed_secs())
                ),
            ],
            RunMode::Post if ok && self.checks_enabled => vec![
                format!(
                    "{glyph} Upload complete — {}/{} articles confirmed",
                    self.confirmed_articles(),
                    self.total_segments
                ),
                format!(
                    "  {} in {} · avg {}/s",
                    format_size(self.done_bytes),
                    format_duration(self.elapsed_secs()),
                    format_size((self.done_bytes as f64 / self.elapsed_secs()) as u64)
                ),
            ],
            RunMode::Post if ok => vec![
                format!(
                    "{glyph} Upload complete — {}/{} articles accepted by the server",
                    self.accepted_articles(),
                    self.total_segments
                ),
                format!(
                    "  Not independently verified · elapsed {}",
                    format_duration(self.elapsed_secs())
                ),
            ],
            RunMode::Post => {
                let count_note = if self.checks_enabled {
                    format!(
                        "{}/{} articles uploaded; {}/{} confirmed available",
                        self.accepted_articles(),
                        self.total_segments,
                        self.confirmed_articles(),
                        self.accepted_articles()
                    )
                } else {
                    format!(
                        "{}/{} articles accepted",
                        self.accepted_articles(),
                        self.total_segments
                    )
                };
                vec![
                    format!(
                        "{glyph} Upload incomplete — {count_note}; {}.",
                        ansi(
                            &format!(
                                "{unresolved} unresolved {}",
                                if unresolved == 1 {
                                    "failure"
                                } else {
                                    "failures"
                                }
                            ),
                            "31"
                        )
                    ),
                    "  Retry with --resume; use -v or the session log for server details."
                        .to_string(),
                ]
            }
            RunMode::DryRun => vec![
                format!(
                    "{glyph} Dry run complete — {}/{} articles prepared.",
                    self.accepted_articles(),
                    self.total_segments
                ),
                format!("  elapsed {}", format_duration(self.elapsed_secs())),
            ],
            RunMode::Par2Only => vec![
                format!(
                    "{glyph} PAR2 generation complete — {}/{} source segments processed.",
                    self.accepted_articles(),
                    self.total_segments
                ),
                format!("  elapsed {}", format_duration(self.elapsed_secs())),
            ],
        };

        if ok && self.mode == RunMode::Post {
            let recovered = self.recovered_retries();
            if recovered > 0 {
                lines.push(format!(
                    "  {}",
                    ansi(
                        &format!(
                            "↻ Recovered automatically after {recovered} temporary {}",
                            if recovered == 1 { "retry" } else { "retries" }
                        ),
                        "33"
                    )
                ));
            }
        }

        if self.check_inconclusive > 0 {
            lines.push(format!(
                "  {}",
                ansi(&inconclusive_label(self.check_inconclusive), "33")
            ));
        }
        if let Some(desc) = &self.failed_description {
            lines.push(format!("  {}", ansi(&format!("⚠ {desc}"), "33")));
        }
        lines
            .into_iter()
            .flat_map(|line| {
                if visible_len(&line) <= width {
                    vec![line]
                } else {
                    wrap(&line, width)
                }
            })
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
            ansi("compressing", "33") // amber
        } else if uploading && self.posting_par2 {
            ansi("uploading PAR2", "35")
        } else if uploading {
            ansi("uploading", "32")
        } else if self.check_active {
            ansi("verifying availability", "34")
        } else if self.recover_active {
            ansi("recovering availability", "33")
        } else if self.par2_compute_active {
            ansi("computing recovery data", "36")
        } else if self.par2_writing_active {
            ansi("writing recovery volumes", "36")
        } else if self.par2_encode_units_done() < self.par2_encode_units_total() {
            ansi("reading sources", "36")
        } else if self.par2_write_active || (!self.files.is_empty() && !self.finished) {
            ansi("writing PAR2", "36")
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

        // Keep proxy routing visible for the entire run instead of sharing the
        // transient status line used by retries and preparation phases.
        if let Some(proxy_status) = &self.proxy_status {
            lines.push(box_top("proxy", body_w));
            lines.push(box_line(&ansi(proxy_status, "36"), body_w));
            lines.push(box_bottom(body_w));
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
                let verified = self
                    .check_checked
                    .saturating_sub(self.check_failed)
                    .saturating_sub(self.check_inconclusive);
                let pending = self.done_segments.saturating_sub(self.check_checked);
                // Colour-match the upload bar's two bands: "verified" in the
                // checked band's colour, "pending" in the upload band's —
                // a legend for the bar that lives right next to the numbers
                // it explains instead of a separate line or header colouring.
                let verified_str =
                    ansi(&format!("{verified} confirmed available"), CHECK_BAND_COLOR);
                let pending_str = ansi(
                    &format!("{pending} pending confirmation"),
                    UPLOAD_BAND_COLOR,
                );
                let mut line1 = if self.check_failed > 0 || self.check_inconclusive > 0 {
                    let mut extra = String::new();
                    if self.check_failed > 0 {
                        extra.push_str(&format!(
                            " · {}",
                            ansi(&format!("{} missing", self.check_failed), "31")
                        ));
                    }
                    if self.check_inconclusive > 0 {
                        extra.push_str(&format!(
                            " · {}",
                            ansi(&format!("{} inconclusive", self.check_inconclusive), "33")
                        ));
                    }
                    format!("{verified_str} · {pending_str}{extra}")
                } else if !self.check_active {
                    format!("{verified_str} · all confirmed available")
                } else {
                    format!("{verified_str} · {pending_str}")
                };
                // Reposts had no counter at all before — the only trace was
                // a `Status` line shared (and instantly overwritten) by
                // every other kind of status message in the app.
                let retries_awaiting_confirmation = self
                    .check_reposted
                    .saturating_sub(self.recovered_check_retries);
                if retries_awaiting_confirmation > 0 {
                    line1.push_str(&format!(
                        " · {}",
                        ansi(
                            &format!(
                                "{retries_awaiting_confirmation} {} awaiting confirmation",
                                if retries_awaiting_confirmation == 1 {
                                    "retry"
                                } else {
                                    "retries"
                                }
                            ),
                            "33",
                        )
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
                lines.push(box_top("availability", body_w));
                lines.push(box_line(&line1, body_w));
                lines.push(box_line(&line2, body_w));
                if self.check_inconclusive > 0 {
                    lines.push(box_line(
                        &ansi(&inconclusive_label(self.check_inconclusive), "33"),
                        body_w,
                    ));
                }
                if let Some((n, m)) = self.check_fast_repost {
                    lines.push(box_line(&fast_repost_label(n, m), body_w));
                }
                lines.push(box_bottom(body_w));
            }

            // --- final recovery pass box ------------------------------------
            // Only ever active once `check_active` above has already gone
            // false (see `poster::post_files_inner`: the streaming check
            // queue fully drains, *then* this bounded pass runs on whatever
            // small tail is left) — the two boxes never show at the same
            // time. Has its own bar (unlike the check box) because, unlike
            // the streaming check, this pass has a known, fixed total up
            // front (`CheckRecoverStarted`), so a real percentage is
            // meaningful here.
            if self.recover_active || (final_draw && self.recover_total > 0) {
                let frac = if self.recover_total > 0 {
                    (self.recover_done as f64 / self.recover_total as f64).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let pct = (frac * 100.0).round() as u64;
                let bar = render_bar(frac, bar_w);
                let line1 = format!(
                    "[{bar}] {pct:>3}%  {}/{} article(s)",
                    self.recover_done, self.recover_total
                );
                let ok_count = self.recover_done.saturating_sub(self.recover_failed);
                let mut line2 = ansi(&format!("{ok_count} recovered"), "33");
                if self.recover_failed > 0 {
                    line2.push_str(&format!(
                        " · {}",
                        ansi(&format!("{} still missing", self.recover_failed), "31")
                    ));
                }
                let elapsed = self.recover_start.elapsed().as_secs_f64().max(0.001);
                line2.push_str(&format!(" · elapsed {}", format_duration(elapsed)));
                lines.push(box_top("recover", body_w));
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
                // Real activity, not just the configured pool size: a check
                // pool sitting entirely on 20-second STAT-retry backoffs
                // used to look identical to one actively working through a
                // backlog — both just said "N check".
                let check_str = if self.check_connections > 0 {
                    let check_active = self
                        .check_conn_state
                        .iter()
                        .filter(|&&s| s == ConnState::Busy)
                        .count();
                    let check_dots = if self.check_connections <= GRID_LIMIT {
                        let mut s = String::from(" ");
                        for st in &self.check_conn_state {
                            s.push_str(&conn_dot(*st));
                        }
                        s
                    } else {
                        String::new()
                    };
                    format!(
                        " ·{check_dots} {check_active}/{} check",
                        self.check_connections
                    )
                } else {
                    String::new()
                };
                let retry_str = if retrying > 0 {
                    format!(
                        " · {}",
                        ansi(&format!("{retrying} temporarily retrying"), "33")
                    )
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
            let failures_str = if self.post_retry_pending > 0 {
                format!(
                    "  {}",
                    ansi(
                        &format!("{} temporarily retrying", self.post_retry_pending),
                        "33"
                    )
                )
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
                    format!(" — pass {}/{}", self.par2_pass_index + 1, self.par2_passes)
                } else {
                    String::new()
                };
                if self.par2_compute_active {
                    format!("Computing recovery data{pass_note}")
                } else if self.par2_writing_active {
                    format!(
                        "Writing recovery volumes{pass_note} · {}/{} slices",
                        self.par2_write_done, self.par2_write_total
                    )
                } else {
                    format!(
                        "Reading sources{pass_note} · {}/{} slices",
                        self.par2_encode_done, self.par2_encode_total
                    )
                }
            } else {
                format!(
                    "Writing recovery volumes · {}/{}",
                    self.par2_write_done, self.par2_write_total
                )
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

        // --- optional status / interrupt / failure note -------------------
        // Both branches below can carry information-dense free text (a
        // long filename plus a network error message; a multi-clause status
        // like the PAR2 memory-budget banner) that easily exceeds one
        // terminal row. `truncate`-ing it to fit used to just cut the back
        // half off with a bare "…" and no way to read the rest. Word-wrap
        // it across a few lines instead — bounded by `STATUS_MAX_LINES` so
        // a genuinely unbounded value (arbitrary hook output, a long list
        // of file names) still can't make the panel grow without limit.
        if let Some(desc) = &self.failed_description {
            lines.extend(wrapped_note("⚠ ", desc, width));
        } else if self.aborted {
            lines.push("⚠ abort — dropping connections, saving resume state".to_string());
        } else if self.interrupted {
            lines.push(
                "⚠ interrupt — finishing in-flight articles · Ctrl+C again to abort".to_string(),
            );
        } else if !self.status.is_empty() {
            let elapsed_str = if let Some(since) = self.status_since {
                format!(" · {}", format_duration(since.elapsed().as_secs_f64()))
            } else {
                String::new()
            };
            let full = format!("{}{}", self.status, elapsed_str);
            let marker = if self.status.starts_with("Temporarily retried") {
                ansi("⚠ ", "33")
            } else {
                "▸ ".to_string()
            };
            lines.extend(wrapped_note(&marker, &full, width));
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

        if !self.plain_failed_printed {
            if let Some(desc) = &self.failed_description {
                self.plain_failed_printed = true;
                let _ = writeln!(err, "⚠ {desc}");
                let _ = err.flush();
            }
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

        if self.par2_compute_active {
            let _ = writeln!(
                err,
                "PAR2: computing recovery data — pass {}/{}",
                self.par2_pass_index + 1,
                self.par2_passes,
            );
            let _ = err.flush();
            return;
        }

        if self.par2_writing_active {
            let _ = writeln!(
                err,
                "PAR2: writing recovery volumes — pass {}/{} · {}/{} slices",
                self.par2_pass_index + 1,
                self.par2_passes,
                self.par2_write_done,
                self.par2_write_total,
            );
            let _ = err.flush();
            return;
        }

        if self.par2_encode_units_done() < self.par2_encode_units_total() {
            let _ = writeln!(
                err,
                "PAR2: Reading sources — pass {}/{} · {}/{} slices",
                self.par2_pass_index + 1,
                self.par2_passes,
                self.par2_encode_done,
                self.par2_encode_total,
            );
            let _ = err.flush();
            return;
        }

        if self.par2_write_active {
            let _ = writeln!(
                err,
                "PAR2: writing recovery volumes · {}/{} slices",
                self.par2_write_done, self.par2_write_total,
            );
            let _ = err.flush();
            return;
        }

        // Final recovery pass — see the TTY panel's "recover" box. Its own
        // early-return branch for the same reason as `compress_active`/
        // `par2_write_active` above: without one, this sequential,
        // potentially multi-minute pass produced no plain-mode output at all
        // once the streaming check's `check_active` suffix below stopped
        // applying (`CheckDone` already fired by the time this starts).
        if self.recover_active {
            let ok_count = self.recover_done.saturating_sub(self.recover_failed);
            let missing_str = if self.recover_failed > 0 {
                format!(" · {} still missing", self.recover_failed)
            } else {
                String::new()
            };
            let _ = writeln!(
                err,
                "recovering availability: {}/{} article(s) · {ok_count} recovered{missing_str}",
                self.recover_done, self.recover_total,
            );
            let _ = err.flush();
            return;
        }

        // Streaming check runs concurrently with the upload, so it's an
        // extra suffix on the normal progress line rather than a phase that
        // suppresses it.
        let check_suffix = if self.check_active || (final_draw && self.check_checked > 0) {
            let verified = self
                .check_checked
                .saturating_sub(self.check_failed)
                .saturating_sub(self.check_inconclusive);
            let pending = self.done_segments.saturating_sub(self.check_checked);
            let mut suffix = format!(
                " · availability: {verified} confirmed/{} missing/{} inconclusive/{pending} pending",
                self.check_failed, self.check_inconclusive
            );
            let retries_awaiting_confirmation = self
                .check_reposted
                .saturating_sub(self.recovered_check_retries);
            if retries_awaiting_confirmation > 0 {
                suffix.push_str(&format!(
                    "/{retries_awaiting_confirmation} awaiting confirmation"
                ));
            }
            // Unlike the boxed panel, plain mode has no dedicated retry line
            // — append it here so a connection-error backoff isn't silently
            // invisible when output is redirected/logged (non-TTY).
            if let Some((label, deadline)) = &self.check_retry {
                let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
                suffix.push_str(&format!(" · {label} in {remaining}s"));
            }
            if self.check_inconclusive > 0 {
                suffix.push_str(&format!(
                    " · {}",
                    inconclusive_label(self.check_inconclusive)
                ));
            }
            if let Some((n, m)) = self.check_fast_repost {
                suffix.push_str(&format!(" · {}", fast_repost_label(n, m)));
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
            for line in self.summary_lines(usize::MAX) {
                let _ = writeln!(err, "{}", strip_ansi_for_plain(&line));
            }
        } else {
            let transient = if self.post_retry_pending > 0 {
                format!(" · {} temporarily retrying", self.post_retry_pending)
            } else {
                String::new()
            };
            let _ = writeln!(
                err,
                "uploading: {}/{} articles · {} · {}/s{transient}{check_suffix}",
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
        ConnState::Retrying => ansi("●", "33"), // amber — temporarily retrying
        ConnState::Idle => ansi("○", "2"),      // dim — drained
    }
}

#[cfg(test)]
#[path = "terminal/tests.rs"]
mod tests;
