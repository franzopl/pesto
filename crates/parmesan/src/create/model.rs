use std::error::Error as StdError;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Selects how much recovery data a new PAR2 set contains.
///
/// Percentage recovery is based on the number of input slices after the
/// slice geometry has been selected. Exact recovery blocks are useful when a
/// caller has a known repair target and must not be combined with a percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// Generate a percentage of recovery blocks relative to input slices.
    Percentage(u8),
    /// Generate exactly this many recovery blocks.
    Blocks(u16),
}

impl Default for Recovery {
    fn default() -> Self {
        Self::Percentage(10)
    }
}

/// Selects the PAR2 input-slice geometry.
///
/// [`SliceStrategy::Automatic`] uses Parmesan's geometry heuristic. Supplying
/// a size or a count opts out of that heuristic for callers that need a
/// predictable layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SliceStrategy {
    /// Select a balanced slice size automatically.
    #[default]
    Automatic,
    /// Use slices of this many bytes.
    Size(usize),
    /// Target this many input slices.
    Count(usize),
}

/// Controls how a creation operation handles existing output files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputPolicy {
    /// Refuse to replace any existing output file.
    #[default]
    FailIfExists,
    /// Replace an existing recovery set with the newly created one.
    ReplaceExisting,
}

/// A cooperative cancellation handle for a PAR2 creation operation.
///
/// Clone the handle before starting creation and call [`Self::cancel`] from
/// another task or thread. Cancellation is observed at input-read and output
/// publication boundaries; staged output is removed before the operation
/// returns.
#[derive(Debug, Clone, Default)]
pub struct CreateCancellation {
    cancelled: Arc<AtomicBool>,
}

/// Stable category of a high-level creation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CreateErrorKind {
    /// The request contains an invalid or unsupported value.
    InvalidRequest,
    /// Cancellation was requested before the recovery set was published.
    Cancelled,
    /// An output file already exists and replacement was not allowed.
    OutputExists {
        /// Conflicting output path.
        path: PathBuf,
    },
    /// A filesystem operation failed.
    Io,
    /// An implementation or PAR2-processing failure that has no more specific category.
    Other,
}

/// Error returned by the high-level creation API.
///
/// Use [`Self::kind`] for programmatic handling and the error display text for
/// an actionable human-readable explanation. The lower-level cause remains
/// available through [`std::error::Error::source`].
#[derive(Debug)]
pub struct CreateError {
    kind: CreateErrorKind,
    source: anyhow::Error,
}

impl CreateError {
    /// Classifies this error without exposing internal creation details.
    pub fn kind(&self) -> &CreateErrorKind {
        &self.kind
    }

    pub(super) fn from_operation(source: anyhow::Error) -> Self {
        let kind = if source.downcast_ref::<CancelledFailure>().is_some() {
            CreateErrorKind::Cancelled
        } else if let Some(output) = source.downcast_ref::<OutputExistsFailure>() {
            CreateErrorKind::OutputExists {
                path: output.path.clone(),
            }
        } else if source.downcast_ref::<InvalidRequestFailure>().is_some() {
            CreateErrorKind::InvalidRequest
        } else if source.chain().any(|error| error.is::<std::io::Error>()) {
            CreateErrorKind::Io
        } else {
            CreateErrorKind::Other
        };
        Self { kind, source }
    }
}

impl fmt::Display for CreateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl StdError for CreateError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug)]
pub(super) struct InvalidRequestFailure(pub(super) &'static str);

impl fmt::Display for InvalidRequestFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl StdError for InvalidRequestFailure {}

#[derive(Debug)]
pub(super) struct CancelledFailure;

impl fmt::Display for CancelledFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PAR2 creation cancelled")
    }
}

impl StdError for CancelledFailure {}

#[derive(Debug)]
pub(super) struct OutputExistsFailure {
    pub(super) path: PathBuf,
}

impl fmt::Display for OutputExistsFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "output file already exists: `{}`",
            self.path.display()
        )
    }
}

impl StdError for OutputExistsFailure {}

impl CreateCancellation {
    /// Creates a cancellation handle in its active state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation of every operation using this handle.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn as_atomic(&self) -> &AtomicBool {
        &self.cancelled
    }
}

/// A path-based request to create a PAR2 recovery set.
///
/// A request is configured with builder-style methods. It deliberately does
/// not expose SIMD implementation choices: the high-level API selects the
/// best supported encoder automatically, while expert callers can continue to
/// use [`crate::encoder::RecoveryEncoder`] directly.
#[derive(Debug, Clone)]
pub struct CreateRequest {
    input_paths: Vec<PathBuf>,
    output_dir: PathBuf,
    base_name: Option<String>,
    recovery: Recovery,
    slice_strategy: SliceStrategy,
    memory_limit: usize,
    threads: Option<NonZeroUsize>,
    output_policy: OutputPolicy,
    recurse: bool,
    creator: Option<String>,
    recovery_offset: u32,
}

impl CreateRequest {
    /// Starts a request for the supplied filesystem paths.
    ///
    /// Directories require [`Self::recurse`] to be enabled. The default output
    /// directory is the current directory, matching the `parmesan create`
    /// command.
    pub fn from_paths<I, P>(paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        Self {
            input_paths: paths
                .into_iter()
                .map(|path| path.as_ref().to_path_buf())
                .collect(),
            output_dir: PathBuf::from("."),
            base_name: None,
            recovery: Recovery::default(),
            slice_strategy: SliceStrategy::default(),
            memory_limit: 1024 * 1024 * 1024,
            threads: None,
            output_policy: OutputPolicy::default(),
            recurse: false,
            creator: None,
            recovery_offset: 0,
        }
    }

    /// Writes the recovery set under `path` instead of the current directory.
    #[must_use]
    pub fn output_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.output_dir = path.as_ref().to_path_buf();
        self
    }

    /// Uses `name` as the common base name of the index and recovery volumes.
    #[must_use]
    pub fn base_name(mut self, name: impl Into<String>) -> Self {
        self.base_name = Some(name.into());
        self
    }

    /// Selects the amount of recovery data to generate.
    #[must_use]
    pub fn recovery(mut self, recovery: Recovery) -> Self {
        self.recovery = recovery;
        self
    }

    /// Selects automatic, size-based, or count-based input slicing.
    #[must_use]
    pub fn slice_strategy(mut self, strategy: SliceStrategy) -> Self {
        self.slice_strategy = strategy;
        self
    }

    /// Limits memory reserved for recovery buffers.
    #[must_use]
    pub fn memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit = bytes;
        self
    }

    /// Requests a private compute pool with this many worker threads.
    #[must_use]
    pub fn threads(mut self, threads: NonZeroUsize) -> Self {
        self.threads = Some(threads);
        self
    }

    /// Selects how existing files in the output directory are handled.
    #[must_use]
    pub fn output_policy(mut self, policy: OutputPolicy) -> Self {
        self.output_policy = policy;
        self
    }

    /// Enables recursive expansion of directory input paths.
    #[must_use]
    pub fn recurse(mut self) -> Self {
        self.recurse = true;
        self
    }

    /// Sets the text stored in the PAR2 Creator packet.
    #[must_use]
    pub fn creator(mut self, creator: impl Into<String>) -> Self {
        self.creator = Some(creator.into());
        self
    }

    /// Starts recovery exponents at `offset` instead of zero.
    #[must_use]
    pub fn recovery_offset(mut self, offset: u32) -> Self {
        self.recovery_offset = offset;
        self
    }

    /// Input paths supplied by the caller.
    pub fn input_paths(&self) -> &[PathBuf] {
        &self.input_paths
    }

    /// Destination directory for generated files.
    pub fn output_directory(&self) -> &Path {
        &self.output_dir
    }

    /// Explicit common output base name, if one was set.
    pub fn output_base_name(&self) -> Option<&str> {
        self.base_name.as_deref()
    }

    /// Requested recovery-data amount.
    pub fn recovery_strategy(&self) -> Recovery {
        self.recovery
    }

    /// Requested input-slice strategy.
    pub fn requested_slice_strategy(&self) -> SliceStrategy {
        self.slice_strategy
    }

    /// Requested recovery-buffer memory limit in bytes.
    pub fn requested_memory_limit(&self) -> usize {
        self.memory_limit
    }

    /// Requested private compute-pool size, or `None` for automatic sizing.
    pub fn requested_threads(&self) -> Option<NonZeroUsize> {
        self.threads
    }

    /// Output collision policy.
    pub fn requested_output_policy(&self) -> OutputPolicy {
        self.output_policy
    }

    /// Whether directory inputs are expanded recursively.
    pub fn recurses(&self) -> bool {
        self.recurse
    }

    /// Explicit Creator packet text, if one was supplied.
    pub fn creator_text(&self) -> Option<&str> {
        self.creator.as_deref()
    }

    /// First requested recovery exponent.
    pub fn requested_recovery_offset(&self) -> u32 {
        self.recovery_offset
    }
}

/// Geometry selected for a completed recovery set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CreateGeometry {
    /// Size of each PAR2 input and recovery slice in bytes.
    pub slice_size: usize,
    /// Number of input slices across all protected files.
    pub input_slices: usize,
    /// Number of recovery blocks generated.
    pub recovery_blocks: usize,
}

/// Structured outcome of a successful creation operation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CreateReport {
    /// PAR2 recovery-set ID computed from the Main packet.
    pub recovery_set_id: [u8; 16],
    /// Geometry selected for this recovery set.
    pub geometry: CreateGeometry,
    /// Written index file, when the request enabled index generation.
    pub index_path: Option<PathBuf>,
    /// Written recovery volume files in layout order.
    pub volume_paths: Vec<PathBuf>,
}

/// A progress event emitted while a recovery set is created.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CreateEvent {
    /// Input discovery and geometry planning completed.
    Planned {
        /// Number of input files that will be protected.
        input_files: usize,
        /// Total byte length of input files before PAR2 padding.
        input_bytes: u64,
        /// Geometry selected for this operation.
        geometry: CreateGeometry,
    },
    /// A recovery-encoding pass began.
    PassStarted {
        /// Zero-based pass number.
        pass: usize,
        /// First recovery exponent produced by this pass.
        first_exponent: u32,
        /// Number of recovery blocks produced by this pass.
        recovery_blocks: usize,
    },
    /// More source bytes were read during the active pass.
    ///
    /// `bytes_read` is cumulative within the active pass. A low-memory
    /// operation can have multiple passes or slice windows and therefore read
    /// an input file more than once.
    BytesRead {
        /// Zero-based active pass number.
        pass: usize,
        /// Cumulative bytes read during this pass.
        bytes_read: u64,
    },
    /// The PAR2 index file was written.
    IndexWritten {
        /// Final index-file path.
        path: PathBuf,
    },
    /// A recovery volume was written.
    VolumeWritten {
        /// Final recovery-volume path.
        path: PathBuf,
    },
    /// Creation stopped before publishing a complete recovery set.
    Cancelled,
}
