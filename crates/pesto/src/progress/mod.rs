//! Progress reporting.
//!
//! The poster never writes to the terminal directly. Instead it emits
//! [`ProgressEvent`]s on a channel, which keeps `pesto` usable as a library:
//! an embedding application (e.g. `upapasta`) drains the channel and renders
//! progress however it likes, while the `pesto` binary installs the built-in
//! [`crate::ui::terminal::spawn_renderer`] panel.
//!
//! Events flow over an *unbounded* channel so emitting one never blocks the
//! hot posting path; dropping the receiver simply makes emission a no-op.

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

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
        /// Number of NNTP connections / worker threads dedicated to posting
        /// (0 for `--par2-only`) — does not include `check_connections`.
        connections: usize,
        /// Number of NNTP connections dedicated to the streaming STAT check
        /// (`poster::check`), carved out of `total_connections()` / `-n`
        /// (not additive, and not a carve-out of this event's `connections`
        /// field — that is `worker_count`, which can be smaller than upload
        /// when the release has few segments). 0 when checking is disabled.
        check_connections: usize,
        /// `host:port` of the NNTP server, or `None` when not posting.
        target: Option<String>,
        /// Exact PAR2 recovery-data size that will be added to the queue
        /// later via `QueueExtended`, computed with the same geometry the
        /// encoder itself uses (not an estimate). Pre-seeded into
        /// `total_bytes` so the bar never jumps when PAR2 files arrive.
        par2_bytes_hint: u64,
        /// Segment count derived from `par2_bytes_hint`, pre-seeded into
        /// `total_segments` for the same reason — without this the
        /// segment-based progress percentage (not the byte one) is what
        /// visibly jumps once PAR2 volumes are queued.
        par2_segments_hint: u64,
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
    /// Persistent SOCKS5 routing information shown separately from transient
    /// upload status notes. The text must never contain proxy credentials.
    ProxyStatus { text: String },
    /// A detailed failure from a producer or a segment that exhausted the
    /// main posting retry budget. Segment failures may still be recovered by
    /// the bounded end-of-run rescue pass.
    Failed { description: String },
    /// A segment exhausted the main worker's retry budget and is queued for
    /// the automatic end-of-run rescue pass. The built-in terminal uses this
    /// to keep the condition amber while existing JSON diagnostics continue
    /// to receive the following `Failed` event unchanged.
    PostRetryQueued,
    /// One or more articles that encountered a transient POST error were
    /// subsequently accepted. `previously_failed` is true when they had
    /// already exhausted the main worker's retry budget (and therefore
    /// emitted `Failed`/`SegmentDone { ok: false }`) before the bounded
    /// end-of-run rescue pass recovered them.
    ///
    /// This is additive terminal-UX metadata. Existing `Failed` and
    /// `SegmentDone` events are deliberately retained unchanged for JSON and
    /// embedding consumers that rely on their diagnostic semantics.
    PostRetryRecovered { count: u64, previously_failed: bool },
    /// Ctrl-C was received; the run is winding down.
    Interrupted,
    /// A second Ctrl-C/SIGTERM, or the graceful-shutdown deadline, dropped
    /// in-flight I/O. Resume state is still persisted before `Finished`.
    Aborted,
    /// An external pause flag was set: every posting worker is suspended at
    /// the next segment-batch boundary, connections kept alive rather than
    /// torn down. See `poster::post_files_inner`'s `external_pause`.
    Paused,
    /// The external pause flag was cleared: posting resumes.
    Resumed,
    /// Terminal event: the run is over.
    Finished,
    /// Archive compression has started. `total_bytes` is the sum of raw input
    /// sizes — a tight bound for the archive in store mode (no compression).
    CompressStarted { total_bytes: u64 },
    /// Archive file on disk has grown to `bytes_written` bytes (polled ~200 ms).
    CompressProgress { bytes_written: u64 },
    /// Compression finished; the archive file is complete.
    CompressDone,
    /// PAR2 encode is about to start; carries configuration for the info block.
    Par2EncodeStarted {
        /// Total source data size in bytes.
        input_bytes: u64,
        /// Number of input slices.
        input_slices: usize,
        /// Number of source files.
        input_files: usize,
        /// Number of recovery blocks.
        recovery_slices: usize,
        /// Size of each slice in bytes.
        slice_size: usize,
        /// Number of input passes (PAR2 spec allows multi-pass encoding).
        passes: usize,
        /// Size of the SIMD processing chunk.
        chunk_size: usize,
        /// Name of the SIMD path used (e.g. "avx2+gfni").
        simd_method: String,
        /// Number of threads in the encoder pool.
        threads: usize,
        /// Soft memory limit for the encoder buffers.
        memory_limit: usize,
    },
    /// Progress update for PAR2 input pass. `done` is the number of slices
    /// processed so far.
    Par2InputProgress { done: usize, total: usize },
    /// The encoder started reading source data for an explicit input pass.
    /// Older consumers can continue inferring pass rollover from
    /// `Par2InputProgress`; the built-in renderer uses this signal to make
    /// multi-pass progress monotonic even if adjacent passes report the same
    /// first counter value.
    Par2PassStarted { pass: usize, passes: usize },
    /// All source slices for this pass have been read and the encoder is
    /// computing recovery data. This phase may be long even though no input
    /// counter changes.
    Par2ComputeStarted { pass: usize, passes: usize },
    /// PAR2 recovery volume write phase has started. `total` is the number of
    /// recovery slices to be written.
    Par2WriteStarted { total: u32 },
    /// One PAR2 recovery slice was written to disk.
    Par2SliceWritten,
    /// A posted article was STAT-checked by the streaming check queue.
    /// `checked` is a running count of articles resolved so far this run
    /// (verified, or given up on after every repost attempt); `ok` is true
    /// if this particular article was confirmed. Fires continuously,
    /// concurrently with the upload — there is no separate "check phase".
    CheckProgress { checked: u64, ok: bool },
    /// STAT itself failed (timeout, transport, 480/502, unexpected code)
    /// until `check_retries` ran out, without ever seeing a 430. `count` is
    /// how many articles have taken this path so far this run; `reason`
    /// matches [`Self::CheckRetrying`] (`"connection error"`).
    CheckInconclusive { count: u64, reason: &'static str },
    /// An isolated first-copy 430 skipped the remaining patient STAT
    /// retries and went straight to repost. `first_checks` / `first_misses`
    /// are the running totals that satisfied the fast-repost heuristic.
    CheckFastRepost {
        first_checks: u64,
        first_misses: u64,
    },
    /// The streaming check queue has fully drained (all articles resolved).
    /// `failed` is MissingConfirmed (STAT 430 exhausted). `inconclusive` is
    /// a failed check path (transport/timeout/480/502/cancel) — not a
    /// confirmed gap, and never "all articles confirmed".
    CheckDone { failed: u64, inconclusive: u64 },
    /// A STAT attempt failed on try `attempt`; retrying after `delay_secs`.
    /// `reason` distinguishes an article genuinely missing ("article not
    /// found") from the STAT call itself failing ("connection error") —
    /// the latter used to be silent (a `tracing::warn!` only, invisible
    /// without `-v`), leaving connection-trouble backoffs indistinguishable
    /// from a hang.
    CheckRetrying {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: &'static str,
    },
    /// An article was successfully reposted under a fresh Message-ID after
    /// its original copy exhausted its STAT attempts. `reposted` is a
    /// running count of reposts so far this run.
    CheckReposted { reposted: u64 },
    /// A verification miss that required a repost was later confirmed
    /// available. Unlike `CheckReposted`, this only fires after STAT succeeds,
    /// so it is safe to include in a recovered-transient success summary.
    CheckRetryRecovered,
    /// The one-time final recovery pass (see `poster::check::recover_missing`)
    /// has started, for a small stubborn tail of articles the streaming
    /// check queue couldn't confirm after every `check_post_retries` round.
    /// `total` is how many articles are in this batch. Unlike the streaming
    /// check, this pass is sequential (one article at a time — repost, wait
    /// `check_delay_secs`, STAT), so it can take a while with no other
    /// visible activity; the renderer gives it its own phase and progress
    /// box instead of leaving the user looking at an idle connection grid.
    CheckRecoverStarted { total: u64 },
    /// One article in the final recovery pass was resolved. `done` is a
    /// running count within this batch (out of `total` from
    /// `CheckRecoverStarted`); `ok` is true when the repost was confirmed
    /// present, false when either the repost itself failed or the final
    /// STAT still couldn't confirm it — both cases used to be completely
    /// silent (a `tracing::warn!` only), fired for every resolution
    /// (success or failure) so the run never goes quiet for the whole
    /// batch.
    CheckRecoverProgress { done: u64, total: u64, ok: bool },
    /// Streaming-check worker connection `conn` (indexed within the check
    /// pool, separate from the upload pool's own `conn` numbering) started
    /// network activity — a STAT, or a repost after a confirmed miss.
    /// Before this, the panel's `conns` line only ever showed the check
    /// pool's *configured* size ("4 check"), never how many of those
    /// connections were actually doing anything at a given moment — a
    /// pool that looked identical whether it was working through a
    /// backlog or sitting on 20-second STAT-retry backoffs.
    CheckConnectionBusy { conn: usize },
    /// The check worker named by [`Self::CheckConnectionBusy`] went back to
    /// waiting for its next ready item (queue empty, or every ready item
    /// still inside its retry/repost delay).
    CheckConnectionIdle { conn: usize },
    /// The check pool grew past the size announced in [`Self::Started`]'s
    /// `check_connections` — see `CheckCoordinatorHandle::scale_up`: once
    /// the upload's own connections go idle, they get reused to help drain
    /// any remaining check backlog instead of sitting unused. `check_connections`
    /// is the pool's new total size, not a delta.
    CheckPoolScaledUp { check_connections: usize },
    /// Worker connection `conn` is authenticating with the server.
    ConnectionAuth { conn: usize },
    /// Worker connection `conn` failed an attempt and is retrying.
    ConnectionRetrying { conn: usize },
    /// Snapshot of the shared buffer pool status.
    BufferPoolStats { total: usize, free: usize },
}

/// Display options for the terminal renderer.
#[derive(Debug, Clone, Default)]
pub struct RendererOptions {
    /// Quiet mode: show a single-line progress summary instead of the full panel.
    pub quiet: bool,
    /// Ring the terminal bell (`\a`) when the run finishes.
    pub bell: bool,
    /// Force the append-only plain renderer even on a TTY. Set when verbose
    /// (`-v`) logs share stderr with the panel: the panel's cursor-movement
    /// redraws and the interleaved log lines would otherwise corrupt each
    /// other, so we fall back to throttled, append-only progress lines that
    /// coexist cleanly with scrolling log output.
    pub plain: bool,
}

mod json;
mod plain;

pub use json::spawn_json_emitter;
pub use plain::{format_size, print_tree, print_upload_flags, UploadFlags};
