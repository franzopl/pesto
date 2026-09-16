//! High-level creation of PAR2 recovery sets.
//!
//! This module is the stable, path-based API for applications that need to
//! create a complete PAR2 recovery set without driving packet serialization,
//! hashing, encoder passes, and volume layout themselves. The low-level
//! modules remain available for specialised integrations.
//!
//! [`create`] is the primary entry point. Its request and result types give
//! the CLI and library one explicit creation contract.

use crate::ops::{
    calculate_geometry, ingest_files_ex, ingest_files_with_progress, plan_memory_layout,
    sort_files_by_file_id, CreateOptions, IngestHashes, InputFile, SliceWindow,
};
use crate::{encoder, encoder::RecoveryEncoder, layout, packet, EncoderLayout, SimdPath};
use anyhow::{Context, Result};
use md5::{Digest, Md5};
use std::error::Error as StdError;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use walkdir::WalkDir;

static NEXT_STAGING_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

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

    fn from_operation(source: anyhow::Error) -> Self {
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
struct InvalidRequestFailure(&'static str);

impl fmt::Display for InvalidRequestFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl StdError for InvalidRequestFailure {}

#[derive(Debug)]
struct CancelledFailure;

impl fmt::Display for CancelledFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PAR2 creation cancelled")
    }
}

impl StdError for CancelledFailure {}

#[derive(Debug)]
struct OutputExistsFailure {
    path: PathBuf,
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

    fn as_atomic(&self) -> &AtomicBool {
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

/// Creates a complete PAR2 recovery set from filesystem paths.
///
/// The operation selects the fastest supported encoder layout automatically,
/// streams input files instead of loading them wholly into memory, and returns
/// every output path on success. It does not write terminal output.
///
/// Use [`create_cancellable`] when another task may need to stop the operation.
///
/// # Examples
///
/// ```no_run
/// use parmesan::create::{create, CreateRequest, Recovery};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let report = create(
///     CreateRequest::from_paths(["movie.mkv"])
///         .output_dir("recovery")
///         .recovery(Recovery::Percentage(10)),
/// )
/// .await?;
///
/// println!("{}", report.index_path.unwrap().display());
/// # Ok(())
/// # }
/// ```
pub async fn create(request: CreateRequest) -> std::result::Result<CreateReport, CreateError> {
    create_with_options(request, EngineOptions::default(), |_| {})
        .await
        .map_err(CreateError::from_operation)
}

/// Creates a recovery set that can be stopped cooperatively.
pub async fn create_cancellable(
    request: CreateRequest,
    cancellation: &CreateCancellation,
) -> std::result::Result<CreateReport, CreateError> {
    create_with_options_and_cancellation(
        request,
        EngineOptions::default(),
        Some(cancellation),
        |_| {},
    )
    .await
    .map_err(CreateError::from_operation)
}

/// Creates a recovery set and synchronously reports structured progress.
///
/// The callback runs on the calling task while input is read. It should remain
/// lightweight; applications that render a UI or perform blocking work should
/// forward events to their own channel. The callback cannot cancel creation in
/// this version of the API.
pub async fn create_with_progress<F>(
    request: CreateRequest,
    on_event: F,
) -> std::result::Result<CreateReport, CreateError>
where
    F: FnMut(CreateEvent),
{
    create_with_options(request, EngineOptions::default(), on_event)
        .await
        .map_err(CreateError::from_operation)
}

/// Creates a recovery set with progress reporting and cooperative cancellation.
pub async fn create_with_progress_and_cancellation<F>(
    request: CreateRequest,
    cancellation: &CreateCancellation,
    on_event: F,
) -> std::result::Result<CreateReport, CreateError>
where
    F: FnMut(CreateEvent),
{
    create_with_options_and_cancellation(
        request,
        EngineOptions::default(),
        Some(cancellation),
        on_event,
    )
    .await
    .map_err(CreateError::from_operation)
}

#[derive(Debug, Clone, Copy)]
#[doc(hidden)]
pub struct EngineOptions {
    #[doc(hidden)]
    pub simd: SimdPath,
    #[doc(hidden)]
    pub layout: EncoderLayout,
    #[doc(hidden)]
    pub write_index: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            simd: SimdPath::Auto,
            layout: EncoderLayout::Smart,
            write_index: true,
        }
    }
}

#[doc(hidden)]
pub async fn create_with_options<F>(
    request: CreateRequest,
    engine: EngineOptions,
    on_event: F,
) -> Result<CreateReport>
where
    F: FnMut(CreateEvent),
{
    create_with_options_and_cancellation(request, engine, None, on_event).await
}

async fn create_with_options_and_cancellation<F>(
    request: CreateRequest,
    engine: EngineOptions,
    cancellation: Option<&CreateCancellation>,
    mut on_event: F,
) -> Result<CreateReport>
where
    F: FnMut(CreateEvent),
{
    check_cancellation(cancellation, &mut on_event)?;
    validate_request(&request)?;

    let mut input_files = collect_files(request.input_paths(), request.recurses())?;
    if input_files.is_empty() {
        return Err(anyhow::Error::new(InvalidRequestFailure(
            "no input files found",
        )));
    }
    check_cancellation(cancellation, &mut on_event)?;

    // The default output base name follows the first file supplied by the
    // caller, independently of the canonical File ID ordering below.
    let default_base_name = input_files[0].display_name.clone();
    tokio::task::block_in_place(|| sort_files_by_file_id(&mut input_files))?;
    check_cancellation(cancellation, &mut on_event)?;

    let options = CreateOptions {
        slice_size: match request.requested_slice_strategy() {
            SliceStrategy::Automatic | SliceStrategy::Count(_) => None,
            SliceStrategy::Size(size) => Some(size),
        },
        slice_count: match request.requested_slice_strategy() {
            SliceStrategy::Count(count) => Some(count),
            SliceStrategy::Automatic | SliceStrategy::Size(_) => None,
        },
        recovery_count: match request.recovery_strategy() {
            Recovery::Percentage(_) => None,
            Recovery::Blocks(count) => Some(usize::from(count)),
        },
        recovery_pct: match request.recovery_strategy() {
            Recovery::Percentage(percent) => percent,
            Recovery::Blocks(_) => 0,
        },
        memory_limit: request.requested_memory_limit(),
        threads: request.requested_threads().map_or(0, NonZeroUsize::get),
        simd: engine.simd,
    };
    let (slice_size, total_slices, recovery_count) = calculate_geometry(&input_files, &options)?;
    if recovery_count == 0 {
        return Err(anyhow::Error::new(InvalidRequestFailure(
            "at least one recovery block is required",
        )));
    }

    let geometry = CreateGeometry {
        slice_size,
        input_slices: total_slices,
        recovery_blocks: recovery_count,
    };
    let input_bytes = input_files.iter().map(|file| file.size).sum();
    on_event(CreateEvent::Planned {
        input_files: input_files.len(),
        input_bytes,
        geometry,
    });

    let rayon_threads = if options.threads > 0 {
        options.threads
    } else {
        crate::performance_core_count()
    };
    let thread_pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(rayon_threads)
            .build()
            .context("creating private PAR2 compute pool")?,
    );

    let out_dir = request.output_directory().to_path_buf();
    if !out_dir.exists() {
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating output directory `{}`", out_dir.display()))?;
    }
    let base_name = request
        .output_base_name()
        .map(ToOwned::to_owned)
        .unwrap_or(default_base_name);
    let creator = request
        .creator_text()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("parmesan/{}", crate::DISPLAY_VERSION));
    check_cancellation(cancellation, &mut on_event)?;
    let outputs = StagedOutputs::prepare(
        &out_dir,
        &base_name,
        recovery_count,
        engine.write_index,
        request.requested_output_policy(),
    )?;
    let index_path = outputs.index_path();
    let volume_paths = outputs.volume_paths();

    let mut all_checksums: Vec<Vec<packet::SliceChecksum>> = vec![Vec::new(); input_files.len()];
    let memory_plan = plan_memory_layout(slice_size, recovery_count, options.memory_limit);
    let mut cursor = 0usize;
    let mut base_packets = Vec::new();
    let mut recovery_set_id = [0u8; 16];

    while cursor < recovery_count {
        check_cancellation(cancellation, &mut on_event)?;
        let count = (recovery_count - cursor).min(memory_plan.recovery_per_pass.max(1));
        let pass = cursor / memory_plan.recovery_per_pass.max(1);
        let first_exponent = request
            .requested_recovery_offset()
            .checked_add(u32::try_from(cursor).expect("recovery count fits in u32"))
            .context("recovery offset exceeds the PAR2 exponent range")?;
        on_event(CreateEvent::PassStarted {
            pass,
            first_exponent,
            recovery_blocks: count,
        });

        let chunked = memory_plan.slice_chunk < slice_size;
        let (recovery_slices, slice_checksums, hashes) = if chunked {
            let mut ingest_hashes = IngestHashes::default();
            let mut accumulated: Vec<Vec<u8>> = vec![Vec::new(); count];
            let mut offset = 0usize;
            let mut bytes_read = 0u64;
            while offset < slice_size {
                check_cancellation(cancellation, &mut on_event)?;
                let len = memory_plan.slice_chunk.min(slice_size - offset);
                let mut encoder =
                    make_encoder(engine.layout, len, total_slices, first_exponent, count);
                encoder = encoder.with_simd_path(options.simd);
                encoder = encoder.with_flush_limit(
                    (options.memory_limit / 4).clamp(256 * 1024 * 1024, 1024 * 1024 * 1024),
                );
                let worker = crate::worker::Par2Worker::spawn_with_thread_pool(
                    encoder,
                    false,
                    crate::worker::DEFAULT_CHANNEL_DEPTH,
                    Arc::clone(&thread_pool),
                );
                let hash_slot = (pass == 0 && offset == 0).then_some(&mut ingest_hashes);
                ingest_files_ex(
                    &input_files,
                    &worker,
                    slice_size,
                    cancellation.map(CreateCancellation::as_atomic),
                    |_| Ok(()),
                    Some(SliceWindow { offset, len }),
                    hash_slot,
                )
                .await?;
                bytes_read += input_bytes;
                on_event(CreateEvent::BytesRead { pass, bytes_read });
                let (part, _, _) = tokio::task::block_in_place(|| worker.finish());
                check_cancellation(cancellation, &mut on_event)?;
                for (index, slice) in part.into_iter().enumerate() {
                    accumulated[index].extend_from_slice(&slice.data);
                }
                offset += len;
            }
            let slices = accumulated
                .into_iter()
                .enumerate()
                .map(|(index, data)| encoder::RecoverySlice {
                    exponent: first_exponent + index as u32,
                    data,
                })
                .collect();
            (slices, ingest_hashes.checksums, ingest_hashes.hashes)
        } else {
            let mut encoder = make_encoder(
                engine.layout,
                slice_size,
                total_slices,
                first_exponent,
                count,
            );
            if pass == 0 {
                encoder = encoder.with_checksums();
            }
            encoder = encoder.with_simd_path(options.simd);
            encoder = encoder.with_flush_limit(
                (options.memory_limit / 4).clamp(256 * 1024 * 1024, 1024 * 1024 * 1024),
            );
            let worker = crate::worker::Par2Worker::spawn_with_thread_pool(
                encoder,
                pass == 0,
                crate::worker::DEFAULT_CHANNEL_DEPTH,
                Arc::clone(&thread_pool),
            );
            let mut bytes_read = 0u64;
            ingest_files_with_progress(
                &input_files,
                &worker,
                slice_size,
                cancellation.map(CreateCancellation::as_atomic),
                |_| Ok(()),
                |read| {
                    bytes_read += read as u64;
                    on_event(CreateEvent::BytesRead { pass, bytes_read });
                    Ok(())
                },
            )
            .await?;
            let finished = tokio::task::block_in_place(|| worker.finish());
            check_cancellation(cancellation, &mut on_event)?;
            finished
        };

        if pass == 0 {
            // Empty files have no input slices, so the worker returns no hash
            // for them. They still need File Description entries; include the
            // MD5 of empty input rather than indexing worker hashes by file
            // position (which would misalign every following file).
            let md5_empty: [u8; 16] = Md5::digest([]).into();
            let mut worker_hashes = hashes.into_iter();
            let all_hashes: Vec<encoder::FileHashes> = input_files
                .iter()
                .map(|file| {
                    if file.size == 0 {
                        encoder::FileHashes {
                            md5_full: md5_empty,
                            md5_16k: md5_empty,
                            length: 0,
                        }
                    } else {
                        worker_hashes
                            .next()
                            .expect("worker returned fewer hashes than non-empty input files")
                    }
                })
                .collect();

            let mut checksum_iter = slice_checksums.into_iter();
            for (index, file) in input_files.iter().enumerate() {
                let count = (file.size as usize).div_ceil(slice_size);
                all_checksums[index] = checksum_iter.by_ref().take(count).collect();
            }

            let file_ids: Vec<_> = input_files
                .iter()
                .enumerate()
                .map(|(index, file)| {
                    packet::compute_file_id(
                        &all_hashes[index].md5_16k,
                        file.size,
                        &file.display_name,
                    )
                })
                .collect();
            let main_body = packet::main_body(slice_size as u64, &file_ids);
            recovery_set_id = packet::recovery_set_id(&main_body);
            base_packets.extend(packet::serialize_packet(
                &recovery_set_id,
                &packet::TYPE_MAIN,
                &main_body,
            ));
            base_packets.extend(packet::serialize_packet(
                &recovery_set_id,
                &packet::TYPE_CREATOR,
                &packet::creator_body(&creator),
            ));
            for (index, file) in input_files.iter().enumerate() {
                let file_id = &file_ids[index];
                base_packets.extend(packet::serialize_packet(
                    &recovery_set_id,
                    &packet::TYPE_FILE_DESC,
                    &packet::file_description_body(
                        file_id,
                        &all_hashes[index].md5_full,
                        &all_hashes[index].md5_16k,
                        file.size,
                        &file.display_name,
                    ),
                ));
                base_packets.extend(packet::serialize_packet(
                    &recovery_set_id,
                    &packet::TYPE_IFSC,
                    &packet::ifsc_body(file_id, &all_checksums[index]),
                ));
            }

            if engine.write_index {
                check_cancellation(cancellation, &mut on_event)?;
                let path = outputs
                    .staged_index_path()
                    .expect("index path exists when index output is enabled");
                std::fs::write(path, &base_packets)
                    .with_context(|| format!("writing `{}`", path.display()))?;
            }
        }

        let volumes = layout::plan_volumes(recovery_count as u32);
        let recovery_packets =
            packet::serialize_recovery_packets(&recovery_set_id, &recovery_slices);
        for (slice, packet) in recovery_slices.iter().zip(recovery_packets) {
            check_cancellation(cancellation, &mut on_event)?;
            let layout_exponent = slice.exponent - request.requested_recovery_offset();
            let volume = volumes
                .iter()
                .find(|volume| {
                    layout_exponent >= volume.first && layout_exponent < volume.first + volume.count
                })
                .expect("volume layout covers every recovery block");
            let path = outputs.staged_volume_path(*volume);

            if layout_exponent == volume.first {
                tokio::fs::write(path, &base_packets).await?;
            }

            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .await?;
            file.write_all(&packet).await?;
        }

        cursor += count;
    }

    check_cancellation(cancellation, &mut on_event)?;
    outputs.commit()?;
    if let Some(path) = &index_path {
        on_event(CreateEvent::IndexWritten { path: path.clone() });
    }
    for path in &volume_paths {
        on_event(CreateEvent::VolumeWritten { path: path.clone() });
    }

    Ok(CreateReport {
        recovery_set_id,
        geometry,
        index_path,
        volume_paths,
    })
}

fn check_cancellation<F>(cancellation: Option<&CreateCancellation>, on_event: &mut F) -> Result<()>
where
    F: FnMut(CreateEvent),
{
    if cancellation.is_some_and(CreateCancellation::is_cancelled) {
        on_event(CreateEvent::Cancelled);
        return Err(anyhow::Error::new(CancelledFailure));
    }
    Ok(())
}

fn validate_request(request: &CreateRequest) -> Result<()> {
    if request.input_paths().is_empty() {
        return Err(anyhow::Error::new(InvalidRequestFailure(
            "at least one input path is required",
        )));
    }
    if request.requested_memory_limit() == 0 {
        return Err(anyhow::Error::new(InvalidRequestFailure(
            "memory limit must be greater than zero",
        )));
    }
    match request.recovery_strategy() {
        Recovery::Percentage(0) => {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "recovery percentage must be greater than zero",
            )))
        }
        Recovery::Blocks(0) => {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "recovery block count must be greater than zero",
            )))
        }
        Recovery::Percentage(_) | Recovery::Blocks(_) => {}
    }
    match request.requested_slice_strategy() {
        SliceStrategy::Size(0) => {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "slice size must be greater than zero",
            )))
        }
        SliceStrategy::Count(0) => {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "slice count must be greater than zero",
            )))
        }
        SliceStrategy::Automatic | SliceStrategy::Size(_) | SliceStrategy::Count(_) => {}
    }
    if let Some(base_name) = request.output_base_name() {
        let mut components = Path::new(base_name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "output base name must be one normal path component",
            )));
        }
    }
    Ok(())
}

#[derive(Debug)]
struct StagedOutput {
    final_path: PathBuf,
    staged_path: PathBuf,
    volume: Option<layout::VolumeChunk>,
}

#[derive(Debug)]
struct StagedOutputs {
    directory: PathBuf,
    index: Option<StagedOutput>,
    volumes: Vec<StagedOutput>,
}

impl StagedOutputs {
    fn prepare(
        output_dir: &Path,
        base_name: &str,
        recovery_count: usize,
        write_index: bool,
        policy: OutputPolicy,
    ) -> Result<Self> {
        let mut final_paths = Vec::new();
        let index_path = write_index.then(|| output_dir.join(layout::index_name(base_name)));
        if let Some(path) = &index_path {
            final_paths.push(path.clone());
        }
        let volumes = layout::plan_volumes(recovery_count as u32);
        final_paths.extend(
            volumes
                .iter()
                .map(|volume| output_dir.join(layout::volume_name(base_name, *volume))),
        );

        if policy == OutputPolicy::FailIfExists {
            if let Some(path) = final_paths.iter().find(|path| path.exists()) {
                return Err(anyhow::Error::new(OutputExistsFailure {
                    path: path.clone(),
                }));
            }
        }

        let directory = create_staging_directory(output_dir)?;
        let index = index_path.map(|final_path| StagedOutput {
            staged_path: directory.join(
                final_path
                    .file_name()
                    .expect("generated index name has a filename"),
            ),
            final_path,
            volume: None,
        });
        let volumes = volumes
            .into_iter()
            .map(|volume| {
                let final_path = output_dir.join(layout::volume_name(base_name, volume));
                StagedOutput {
                    staged_path: directory.join(
                        final_path
                            .file_name()
                            .expect("generated volume name has a filename"),
                    ),
                    final_path,
                    volume: Some(volume),
                }
            })
            .collect();

        Ok(Self {
            directory,
            index,
            volumes,
        })
    }

    fn index_path(&self) -> Option<PathBuf> {
        self.index.as_ref().map(|output| output.final_path.clone())
    }

    fn volume_paths(&self) -> Vec<PathBuf> {
        self.volumes
            .iter()
            .map(|output| output.final_path.clone())
            .collect()
    }

    fn staged_index_path(&self) -> Option<&Path> {
        self.index
            .as_ref()
            .map(|output| output.staged_path.as_path())
    }

    fn staged_volume_path(&self, volume: layout::VolumeChunk) -> &Path {
        self.volumes
            .iter()
            .find(|output| output.volume == Some(volume))
            .map(|output| output.staged_path.as_path())
            .expect("a staged path exists for every planned recovery volume")
    }

    fn commit(self) -> Result<()> {
        let mut outputs: Vec<&StagedOutput> = self.volumes.iter().collect();
        if let Some(index) = &self.index {
            outputs.push(index);
        }
        for output in outputs {
            std::fs::rename(&output.staged_path, &output.final_path).with_context(|| {
                format!(
                    "publishing staged output `{}` as `{}`",
                    output.staged_path.display(),
                    output.final_path.display()
                )
            })?;
        }
        std::fs::remove_dir(&self.directory).with_context(|| {
            format!("removing staging directory `{}`", self.directory.display())
        })?;
        Ok(())
    }
}

impl Drop for StagedOutputs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn create_staging_directory(output_dir: &Path) -> Result<PathBuf> {
    const MAX_ATTEMPTS: usize = 1024;
    for _ in 0..MAX_ATTEMPTS {
        let sequence = NEXT_STAGING_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = output_dir.join(format!(
            ".parmesan-create-{}-{sequence}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating staging directory `{}`", path.display()))
            }
        }
    }
    anyhow::bail!("could not allocate a unique PAR2 staging directory")
}

fn collect_files(paths: &[PathBuf], recurse: bool) -> Result<Vec<InputFile>> {
    let mut input_files = Vec::new();
    for path in paths {
        let metadata =
            std::fs::metadata(path).with_context(|| format!("stat `{}`", path.display()))?;
        if metadata.is_dir() {
            if !recurse {
                anyhow::bail!(
                    "`{}` is a directory; enable recursive expansion to protect it",
                    path.display()
                );
            }
            for entry in WalkDir::new(path)
                .follow_links(false)
                .sort_by_file_name()
                .into_iter()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_file())
            {
                let size = entry.metadata()?.len();
                input_files.push(InputFile {
                    path: entry.path().to_path_buf(),
                    display_name: entry.file_name().to_string_lossy().into_owned(),
                    size,
                });
            }
        } else {
            input_files.push(InputFile {
                path: path.clone(),
                display_name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                size: metadata.len(),
            });
        }
    }
    Ok(input_files)
}

fn make_encoder(
    layout: EncoderLayout,
    slice_size: usize,
    total_slices: usize,
    first_exponent: u32,
    count: usize,
) -> RecoveryEncoder {
    match layout {
        EncoderLayout::Smart => {
            RecoveryEncoder::new_smart(slice_size, total_slices, first_exponent, count)
        }
        EncoderLayout::Normal => {
            RecoveryEncoder::new(slice_size, total_slices, first_exponent, count)
        }
        EncoderLayout::Affine => {
            RecoveryEncoder::new_affine(slice_size, total_slices, first_exponent, count)
        }
        EncoderLayout::Affine512 => {
            RecoveryEncoder::new_affine512(slice_size, total_slices, first_exponent, count)
        }
        EncoderLayout::Shuffle2x => {
            RecoveryEncoder::new_shuffle2x(slice_size, total_slices, first_exponent, count)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEMP_DIR: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "parmesan_create_{label}_{}_{}",
            std::process::id(),
            sequence
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn request_defaults_match_the_cli_create_defaults() {
        let request = CreateRequest::from_paths(["movie.mkv"]);

        assert_eq!(request.input_paths(), [PathBuf::from("movie.mkv")]);
        assert_eq!(request.output_directory(), Path::new("."));
        assert_eq!(request.recovery_strategy(), Recovery::Percentage(10));
        assert_eq!(request.requested_slice_strategy(), SliceStrategy::Automatic);
        assert_eq!(request.requested_memory_limit(), 1024 * 1024 * 1024);
        assert_eq!(request.requested_threads(), None);
        assert_eq!(
            request.requested_output_policy(),
            OutputPolicy::FailIfExists
        );
        assert!(!request.recurses());
        assert_eq!(request.requested_recovery_offset(), 0);
    }

    #[test]
    fn request_builders_preserve_all_explicit_choices() {
        let threads = NonZeroUsize::new(4).unwrap();
        let request = CreateRequest::from_paths(["one.bin", "two.bin"])
            .output_dir("out")
            .base_name("archive")
            .recovery(Recovery::Blocks(24))
            .slice_strategy(SliceStrategy::Size(1024))
            .memory_limit(4096)
            .threads(threads)
            .output_policy(OutputPolicy::ReplaceExisting)
            .recurse()
            .creator("example-app/1.0")
            .recovery_offset(9);

        assert_eq!(request.output_directory(), Path::new("out"));
        assert_eq!(request.output_base_name(), Some("archive"));
        assert_eq!(request.recovery_strategy(), Recovery::Blocks(24));
        assert_eq!(
            request.requested_slice_strategy(),
            SliceStrategy::Size(1024)
        );
        assert_eq!(request.requested_memory_limit(), 4096);
        assert_eq!(request.requested_threads(), Some(threads));
        assert_eq!(
            request.requested_output_policy(),
            OutputPolicy::ReplaceExisting
        );
        assert!(request.recurses());
        assert_eq!(request.creator_text(), Some("example-app/1.0"));
        assert_eq!(request.requested_recovery_offset(), 9);
    }

    #[test]
    fn public_create_writes_and_reports_a_complete_recovery_set() {
        let root = temp_dir("public_api");
        let input = root.join("input.bin");
        let output = root.join("output");
        std::fs::write(&input, b"a small but complete public API fixture").unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut events = Vec::new();
        let report = runtime
            .block_on(create_with_progress(
                CreateRequest::from_paths([&input])
                    .output_dir(&output)
                    .base_name("fixture")
                    .recovery(Recovery::Blocks(1))
                    .slice_strategy(SliceStrategy::Size(64)),
                |event| events.push(event),
            ))
            .unwrap();

        assert_eq!(report.geometry.slice_size, 64);
        assert_eq!(report.geometry.input_slices, 1);
        assert_eq!(report.geometry.recovery_blocks, 1);
        assert_eq!(report.index_path, Some(output.join("fixture.par2")));
        assert_eq!(
            report.volume_paths,
            [output.join("fixture.vol000+001.par2")]
        );
        assert!(report.index_path.as_ref().unwrap().is_file());
        assert!(report.volume_paths[0].is_file());
        assert!(events
            .iter()
            .any(|event| matches!(event, CreateEvent::Planned { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, CreateEvent::BytesRead { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, CreateEvent::IndexWritten { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, CreateEvent::VolumeWritten { .. })));
        let mut output_entries: Vec<_> = std::fs::read_dir(&output)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        output_entries.sort();
        assert_eq!(
            output_entries,
            ["fixture.par2", "fixture.vol000+001.par2"].map(std::ffi::OsString::from)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_output_is_rejected_before_a_staging_directory_is_created() {
        let root = temp_dir("collision");
        let input = root.join("input.bin");
        let output = root.join("output");
        std::fs::write(&input, b"fixture").unwrap();
        std::fs::create_dir(&output).unwrap();
        let index = output.join("fixture.par2");
        std::fs::write(&index, b"keep this existing set").unwrap();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(create(
                CreateRequest::from_paths([&input])
                    .output_dir(&output)
                    .base_name("fixture")
                    .recovery(Recovery::Blocks(1)),
            ))
            .unwrap_err();

        assert!(error.to_string().contains("output file already exists"));
        assert_eq!(
            error.kind(),
            &CreateErrorKind::OutputExists {
                path: index.clone()
            }
        );
        assert_eq!(std::fs::read(&index).unwrap(), b"keep this existing set");
        let mut output_entries: Vec<_> = std::fs::read_dir(&output)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        output_entries.sort();
        assert_eq!(
            output_entries,
            ["fixture.par2"].map(std::ffi::OsString::from)
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_request_writes_no_output_and_emits_a_cancelled_event() {
        let root = temp_dir("cancelled");
        let input = root.join("input.bin");
        let output = root.join("output");
        std::fs::write(&input, b"fixture").unwrap();
        let cancellation = CreateCancellation::new();
        cancellation.cancel();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let mut events = Vec::new();
        let error = runtime
            .block_on(create_with_progress_and_cancellation(
                CreateRequest::from_paths([&input])
                    .output_dir(&output)
                    .base_name("fixture")
                    .recovery(Recovery::Blocks(1)),
                &cancellation,
                |event| events.push(event),
            ))
            .unwrap_err();

        assert!(error.to_string().contains("creation cancelled"));
        assert_eq!(error.kind(), &CreateErrorKind::Cancelled);
        assert_eq!(events, [CreateEvent::Cancelled]);
        assert!(!output.exists());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancellation_after_reading_input_removes_staged_output() {
        let root = temp_dir("cancel_during_create");
        let input = root.join("input.bin");
        let output = root.join("output");
        std::fs::write(&input, [0x3c; 64]).unwrap();
        let cancellation = CreateCancellation::new();
        let cancellation_for_event = cancellation.clone();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(create_with_progress_and_cancellation(
                CreateRequest::from_paths([&input])
                    .output_dir(&output)
                    .base_name("fixture")
                    .recovery(Recovery::Blocks(1))
                    .slice_strategy(SliceStrategy::Size(64)),
                &cancellation,
                move |event| {
                    if matches!(event, CreateEvent::BytesRead { .. }) {
                        cancellation_for_event.cancel();
                    }
                },
            ))
            .unwrap_err();

        assert_eq!(error.kind(), &CreateErrorKind::Cancelled);
        assert!(output.is_dir());
        assert!(std::fs::read_dir(&output).unwrap().next().is_none());

        std::fs::remove_dir_all(root).unwrap();
    }
}
