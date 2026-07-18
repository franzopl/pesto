//! Progress events emitted while downloading, mirroring the shape of
//! [`pesto::progress`] so a future TUI/web frontend can consume both engines
//! the same way.

use tokio::sync::mpsc;

/// One file in the run, as announced by [`ProgressEvent::Started`].
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub segments: u32,
    pub bytes: u64,
}

/// A single progress update.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// The run begins. Carries the full work plan, mirroring
    /// [`pesto::progress::ProgressEvent::Started`] so a future TUI/web
    /// frontend can seed totals from the event stream alone, without a
    /// side-channel argument.
    Started { files: Vec<FileEntry> },
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
