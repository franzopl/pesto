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

use std::io::Write;
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
        /// Best-effort estimate of PAR2 bytes that will be added to the queue
        /// later via `QueueExtended`. Pre-seeded into `total_bytes` so the bar
        /// never goes backwards when PAR2 files arrive.
        par2_bytes_hint: u64,
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
    /// PAR2 recovery volume write phase has started. `total` is the number of
    /// recovery slices to be written.
    Par2WriteStarted { total: u32 },
    /// One PAR2 recovery slice was written to disk.
    Par2SliceWritten,
    /// Post-upload consistency check (STAT) started. `total` is the number of
    /// articles to check.
    CheckStarted { total: u64 },
    /// Progress update for the post-upload check. `checked` is the number of
    /// articles processed; `ok` is true if the article exists on the server.
    CheckProgress { checked: u64, ok: bool },
    /// Post-upload check finished. `failed` is the number of missing articles.
    CheckDone { failed: u64 },
    /// An article was not found on attempt `attempt`; retrying after `delay_secs`.
    CheckRetrying {
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
    },
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
                    ProgressEvent::Par2EncodeStarted { .. } => {}
                    ProgressEvent::Par2InputProgress { .. } => {}
                    ProgressEvent::Par2WriteStarted { total } => {
                        let _ = writeln!(out, r#"{{"type":"par2_write_started","total":{total}}}"#);
                    }
                    ProgressEvent::Par2SliceWritten => {
                        let _ = writeln!(out, r#"{{"type":"par2_slice_written"}}"#);
                    }
                    ProgressEvent::CheckStarted { total } => {
                        let _ = writeln!(out, r#"{{"type":"check_started","total":{total}}}"#);
                    }
                    ProgressEvent::CheckProgress { checked, ok } => {
                        let ok_str = if ok { "true" } else { "false" };
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_progress","checked":{checked},"ok":{ok_str}}}"#
                        );
                    }
                    ProgressEvent::CheckDone { failed } => {
                        let _ = writeln!(out, r#"{{"type":"check_done","failed":{failed}}}"#);
                    }
                    ProgressEvent::CheckRetrying {
                        attempt,
                        max_attempts,
                        delay_secs,
                    } => {
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_retrying","attempt":{attempt},"max_attempts":{max_attempts},"delay_secs":{delay_secs}}}"#
                        );
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

/// Print a `tree`-style breakdown of the upload payload to stderr.
pub fn print_tree(files: &[crate::walk::InputFile]) {
    use std::collections::BTreeMap;

    if files.is_empty() {
        return;
    }

    struct Leaf {
        filename: String,
        size: u64,
    }
    let mut tree: BTreeMap<String, Vec<Leaf>> = BTreeMap::new();
    let mut total_bytes: u64 = 0;

    for f in files {
        let size = std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0);
        total_bytes += size;
        let parts: Vec<&str> = f.name.splitn(2, '/').collect();
        let (dir, filename) = if parts.len() == 2 {
            (parts[0].to_string(), parts[1].to_string())
        } else {
            (String::new(), f.name.clone())
        };
        tree.entry(dir).or_default().push(Leaf { filename, size });
    }

    let dirs: Vec<_> = tree.keys().cloned().collect();
    let dir_count = dirs.len();

    for (di, dir) in dirs.iter().enumerate() {
        let leaves = &tree[dir];
        let is_last_dir = di == dir_count - 1;
        let dir_connector = if is_last_dir {
            "└──"
        } else {
            "├──"
        };

        if dir.is_empty() {
            for (li, leaf) in leaves.iter().enumerate() {
                let is_last = li == leaves.len() - 1;
                let conn = if is_last { "└──" } else { "├──" };
                eprintln!("{conn} {} ({})", leaf.filename, format_size(leaf.size));
            }
        } else {
            eprintln!("{dir_connector} {dir}/");
            let prefix = if is_last_dir { "    " } else { "│   " };
            for (li, leaf) in leaves.iter().enumerate() {
                let is_last = li == leaves.len() - 1;
                let conn = if is_last { "└──" } else { "├──" };
                eprintln!(
                    "{prefix}{conn} {} ({})",
                    leaf.filename,
                    format_size(leaf.size)
                );
            }
        }
    }

    eprintln!();
    eprintln!(
        "  {} file{} · {}",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        format_size(total_bytes)
    );
    eprintln!();
}
