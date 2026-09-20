use crate::progress::RunMode;
use crate::ui::metrics;
use std::collections::HashMap;
use std::time::Instant;

/// Visual state of a single NNTP connection worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum ConnState {
    #[default]
    Idle,
    Busy,
    Auth,
    Retrying,
}

impl RenderState {
    /// Files that have every segment done, and files partially in flight.
    pub(super) fn file_tally(&self) -> (usize, usize) {
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

    pub(super) fn elapsed_secs(&self) -> f64 {
        metrics::elapsed_secs(self.start)
    }

    /// Fraction of the run completed, measured in segments.
    ///
    /// Segments — not bytes — are the single source of truth for every
    /// percentage on screen. `total_bytes` carries the pre-seeded
    /// `par2_bytes_hint`, whose unconsumed remainder means a byte ratio tops
    /// out slightly short of 1.0 (`-q` visibly froze at 95% while the panel
    /// read `100%  864/864 seg`). Bytes stay the basis for speed, size and
    /// ETA, where that remainder is harmless.
    pub(super) fn progress_frac(&self) -> f64 {
        metrics::progress_fraction(self.done_segments, self.total_segments)
    }

    /// Bytes posted per second so far.
    pub(super) fn rate(&self) -> f64 {
        metrics::bytes_per_second(self.done_bytes, self.elapsed_secs())
    }

    /// Record a per-tick speed sample in the ring buffer (phase 21c/21d).
    pub(super) fn push_speed_sample(&mut self, bps: f64) {
        self.speed_history[self.speed_history_pos] = bps;
        self.speed_history_pos = (self.speed_history_pos + 1) % 10;
        if self.speed_history_len < 10 {
            self.speed_history_len += 1;
        }
    }

    /// Return the active speed history slice in chronological order.
    pub(super) fn speed_samples(&self) -> Vec<f64> {
        metrics::ordered_samples(
            &self.speed_history,
            self.speed_history_pos,
            self.speed_history_len,
        )
    }

    /// Compute ETA as a range based on throughput confidence (phase 21d).
    ///
    /// Returns `(low_secs, high_secs, unstable)`.
    pub(super) fn eta_range(&self) -> Option<(f64, f64, bool)> {
        let samples = self.speed_samples();
        metrics::eta_range(self.total_bytes.saturating_sub(self.done_bytes), &samples)
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
    pub(super) fn update_par2_rate_emas(&mut self) {
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
    pub(super) fn par2_encode_units_done(&self) -> usize {
        self.par2_pass_index * self.par2_encode_total + self.par2_encode_done
    }

    /// Total input-slice feeds the encode will perform across every pass.
    pub(super) fn par2_encode_units_total(&self) -> usize {
        self.par2_passes.max(1) * self.par2_encode_total
    }

    pub(super) fn recovered_retries(&self) -> u64 {
        self.recovered_post_retries + self.recovered_check_retries
    }

    pub(super) fn confirmed_articles(&self) -> u64 {
        self.check_checked
            .saturating_sub(self.check_failed)
            .saturating_sub(self.check_inconclusive)
    }

    pub(super) fn accepted_articles(&self) -> u64 {
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
    pub(super) fn par2_encode_remaining_secs(&self) -> Option<f64> {
        let (done, total) = (
            self.par2_encode_units_done(),
            self.par2_encode_units_total(),
        );
        metrics::remaining_secs(done as u64, total as u64, self.par2_encode_rate_ema)
    }

    /// Same idea as [`Self::par2_encode_remaining_secs`], for the (usually
    /// short) phase that writes already-computed recovery data to disk.
    pub(super) fn par2_write_remaining_secs(&self) -> Option<f64> {
        metrics::remaining_secs(
            u64::from(self.par2_write_done),
            u64::from(self.par2_write_total),
            self.par2_write_rate_ema,
        )
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
    pub(super) fn overall_eta_secs(&self) -> Option<(f64, bool)> {
        let rate = self.rate();
        let fallback = (rate > 1.0 && self.total_bytes > self.done_bytes)
            .then(|| (self.total_bytes - self.done_bytes) as f64 / rate);
        metrics::overall_eta(
            self.eta_range(),
            fallback,
            [
                self.par2_encode_remaining_secs(),
                self.par2_write_remaining_secs(),
            ],
        )
    }
}

/// Mutable view built from the terminal progress event stream.
#[allow(dead_code)]
pub(super) struct RenderState {
    pub(super) started: bool,
    pub(super) finished: bool,
    pub(super) mode: RunMode,
    pub(super) target: Option<String>,
    pub(super) start: Instant,
    pub(super) total_segments: u64,
    pub(super) total_bytes: u64,
    pub(super) done_segments: u64,
    pub(super) done_bytes: u64,
    pub(super) failures: u64,
    /// POST failures still eligible for the automatic end-of-run rescue pass.
    pub(super) post_retry_pending: u64,
    /// Articles accepted after a transient POST failure.
    pub(super) recovered_post_retries: u64,
    /// Verification misses whose repost was later confirmed available.
    pub(super) recovered_check_retries: u64,
    /// Number of typed retry-queue signals awaiting their detailed `Failed` event.
    pub(super) post_retry_queued_events: u64,
    pub(super) interrupted: bool,
    pub(super) aborted: bool,
    /// Set by `ProgressEvent::Failed` (e.g. a producer error such as the
    /// `--memory-limit` address-space check bailing). Printed alongside the
    /// "interrupted" note so a run that dies before posting anything doesn't
    /// leave the user with no clue why.
    pub(super) failed_description: Option<String>,
    pub(super) status: String,
    /// Persistent SOCKS5 route indicator, independent of transient status notes.
    pub(super) proxy_status: Option<String>,
    /// When the current non-empty status text was first set.
    pub(super) status_since: Option<Instant>,
    /// Last `status` text printed by `draw_plain` — lets it print each new
    /// status line exactly once instead of never (every phase branch
    /// `return`s before reaching a generic status print) or every tick.
    pub(super) plain_status_printed: String,
    /// Whether `failed_description` has already been printed in plain mode —
    /// mirrors `plain_status_printed`'s one-shot pattern.
    pub(super) plain_failed_printed: bool,
    /// Whether the one-time "connections: N upload · M check" line has
    /// already been printed in plain (non-TTY) mode.
    pub(super) plain_connections_printed: bool,
    /// Number of NNTP connections dedicated to the streaming STAT check,
    /// separate from `conn_files`/`conn_state` (upload connections only).
    /// 0 when checking is disabled. Grows past the `Started`-announced
    /// value on `CheckPoolScaledUp` (see `check::CheckCoordinatorHandle::
    /// scale_up`), so this is always the pool's *current* size.
    pub(super) check_connections: usize,
    /// Whether independent availability checks were enabled for this run.
    pub(super) checks_enabled: bool,
    /// Busy/idle state of each check-pool connection, indexed the same way
    /// as `conn_state` but in the check pool's own numbering — a connection
    /// is `Busy` only while actually doing a STAT or repost, `Idle`
    /// otherwise (polling an empty queue, or every ready item still inside
    /// its retry/repost delay). Without this the `conns` line only ever
    /// showed the check pool's *configured* size, with no way to tell a
    /// pool working through a backlog from one sitting on backoffs.
    pub(super) check_conn_state: Vec<ConnState>,
    /// File currently posted by each worker connection (`None` = idle).
    pub(super) conn_files: Vec<Option<String>>,
    /// Per-file `(done, total)` segment counts, for the file tally.
    pub(super) files: HashMap<String, (u64, u64)>,
    /// Lines emitted by the previous panel draw, to be cleared on the next.
    pub(super) lines_drawn: usize,
    /// Tick counter that paces the non-TTY plain output.
    pub(super) plain_ticks: u32,
    /// Rolling window of bytes-per-second samples (up to 10 entries).
    pub(super) speed_history: [f64; 10],
    pub(super) speed_history_pos: usize,
    pub(super) speed_history_len: usize,
    /// Bytes done at the last tick, for computing per-tick delta.
    pub(super) prev_done_bytes: u64,
    /// Spinner frame index for quiet mode.
    pub(super) spinner_frame: usize,
    /// Connection state overrides: None=normal, Some(ConnState).
    pub(super) conn_state: Vec<ConnState>,
    /// Buffer pool snapshot.
    pub(super) buf_total: usize,
    pub(super) buf_free: usize,
    /// PAR2 bytes hint included in total_bytes upfront; reduced as QueueExtended arrives.
    pub(super) par2_hint_remaining: u64,
    /// PAR2 segments hint included in total_segments upfront; reduced as
    /// QueueExtended arrives, mirroring `par2_hint_remaining` for bytes.
    pub(super) par2_segment_hint_remaining: u64,
    /// Whether any QueueExtended event was received (PAR2 files being posted).
    pub(super) posting_par2: bool,
    /// Whether `-v` is active. Diagnostics that are noise during a healthy
    /// run (the process RSS/CPU line) are gated on it, and the `/proc/self`
    /// polling that feeds them is skipped entirely otherwise.
    pub(super) verbose: bool,
    // Process resource stats (polled from /proc/self on Linux)
    pub(super) proc_rss_bytes: u64,
    pub(super) proc_cpu_pct: f64,
    /// Previous (utime+stime) ticks for CPU delta.
    pub(super) proc_prev_ticks: u64,
    pub(super) proc_prev_tick_time: Instant,
    // Compression phase
    pub(super) compress_active: bool,
    pub(super) compress_total: u64,
    pub(super) compress_written: u64,
    pub(super) compress_start: Instant,
    // PAR2 recovery slice writing phase
    pub(super) par2_write_active: bool,
    pub(super) par2_write_total: u32,
    pub(super) par2_write_done: u32,
    pub(super) par2_write_start: Instant,
    /// Recovery-slice count announced upfront by `Par2EncodeStarted`, so the
    /// combined encode+write progress bar knows the write phase's size before
    /// `Par2WriteStarted` fires. Without it the bar's denominator would grow
    /// when writing begins, jumping the fraction backward. Usually equal to
    /// `par2_write_total`; kept separately because the write total only
    /// becomes authoritative at `Par2WriteStarted`.
    pub(super) par2_recovery_total: usize,
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
    pub(super) par2_passes: usize,
    /// Zero-based index of the pass currently being fed, inferred from
    /// `Par2InputProgress.done` going backwards (the event carries no pass
    /// number of its own).
    pub(super) par2_pass_index: usize,
    /// True once `Par2PassStarted` supplies an authoritative pass index.
    pub(super) par2_explicit_pass: bool,
    /// Source reading has finished for the current pass and recovery math is running.
    pub(super) par2_compute_active: bool,
    /// Recovery packets from the current pass are being flushed to volumes.
    pub(super) par2_writing_active: bool,
    /// `par2_write_done` at the last tick and a smoothed (EMA) slices/sec
    /// rate derived from it — see `par2_write_remaining_secs` for why a
    /// recent rate is used instead of the cumulative since-start average.
    pub(super) prev_par2_write_done: u32,
    pub(super) par2_write_rate_ema: f64,
    // Streaming check queue — runs concurrently with the upload, so there is
    // no fixed total known upfront (unlike the old end-of-run STAT sweep).
    pub(super) check_active: bool,
    pub(super) check_checked: u64,
    pub(super) check_failed: u64,
    /// Articles whose STAT path failed (transport/timeout/unexpected code)
    /// rather than a confirmed 430. Distinct from `check_failed`.
    pub(super) check_inconclusive: u64,
    pub(super) check_reposted: u64,
    pub(super) check_start: Instant,
    /// Latest fast-repost heuristic snapshot, shown until the run ends.
    pub(super) check_fast_repost: Option<(u64, u64)>,
    /// Most recent retry backoff still in its window: (label without the
    /// countdown, e.g. "connection error — retry 1/3", deadline). Cleared
    /// once the deadline passes (`expire_check_retry`) rather than on every
    /// resolved article, so a fast-moving pool of concurrent check workers
    /// doesn't wipe it before the user can read it.
    pub(super) check_retry: Option<(String, Instant)>,
    // Final automatic recovery pass (`poster::check::recover_missing`) — runs
    // strictly *after* the streaming check queue has fully drained
    // (`check_active` is already false by the time this starts), for a
    // small, bounded tail of articles that never got confirmed. It has its
    // own phase/box rather than reusing the check box: it is sequential
    // (one article at a time), so without a dedicated indicator the panel
    // showed a 100% upload bar, an idle connection grid, and nothing else
    // moving for however long the batch took.
    pub(super) recover_active: bool,
    pub(super) recover_done: u64,
    pub(super) recover_total: u64,
    /// Resolutions in this batch that came back `ok: false` (repost failed,
    /// or the final STAT still couldn't confirm it).
    pub(super) recover_failed: u64,
    pub(super) recover_start: Instant,
    // PAR2 input slice encode progress
    pub(super) par2_encode_done: usize,
    pub(super) par2_encode_total: usize,
    pub(super) par2_encode_start: Instant,
    /// `par2_encode_done` at the last tick and a smoothed (EMA) slices/sec
    /// rate derived from it — see `par2_encode_remaining_secs`.
    pub(super) prev_par2_encode_done: usize,
    pub(super) par2_encode_rate_ema: f64,
}

impl RenderState {
    pub(super) fn new() -> Self {
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
}
