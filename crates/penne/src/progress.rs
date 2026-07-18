//! Progress events emitted while downloading, mirroring the shape of
//! [`pesto::progress`] so a future TUI/web frontend can consume both engines
//! the same way.

use tokio::sync::mpsc;

/// A single progress update.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// An article body was fetched successfully.
    SegmentDownloaded {
        file_name: String,
        part: u32,
        bytes: u64,
    },
    /// An article could not be fetched from any configured server.
    SegmentMissing { file_name: String, part: u32 },
    /// An article was fetched but could not be decoded as yEnc from any
    /// server that had it (truncated/corrupted transfer).
    SegmentCorrupt {
        file_name: String,
        part: u32,
        error: String,
    },
    /// A file finished reassembly.
    FileAssembled { file_name: String },
}

/// Sending half, held by the download engine.
pub type ProgressSender = mpsc::UnboundedSender<ProgressEvent>;
/// Receiving half, drained by the CLI/TUI/web frontend.
pub type ProgressReceiver = mpsc::UnboundedReceiver<ProgressEvent>;

/// Create a fresh progress channel.
pub fn channel() -> (ProgressSender, ProgressReceiver) {
    mpsc::unbounded_channel()
}
