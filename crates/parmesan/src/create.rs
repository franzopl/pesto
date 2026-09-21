//! High-level creation of PAR2 recovery sets.
//!
//! This module is the stable, path-based API for applications that need to
//! create a complete PAR2 recovery set without driving packet serialization,
//! hashing, encoder passes, and volume layout themselves. The low-level
//! modules remain available for specialised integrations.
//!
//! [`create`] is the primary entry point. Its request and result types give
//! the CLI and library one explicit creation contract.
//!
//! Module map: `model` holds the request/report/event contracts, `plan`
//! validates and discovers inputs, `ingest` runs the encoder passes,
//! `packet_output` assembles and writes packets, and `output` stages
//! transactional publication.

use crate::ops::{calculate_geometry, plan_memory_layout, sort_files_by_file_id, CreateOptions};
use crate::{packet, EncoderLayout, SimdPath};
use anyhow::{Context, Result};
use std::num::NonZeroUsize;
use std::sync::Arc;

mod ingest;
mod model;
mod output;
mod packet_output;
mod plan;

use model::{CancelledFailure, InvalidRequestFailure};
pub use model::{
    CreateCancellation, CreateError, CreateErrorKind, CreateEvent, CreateGeometry, CreateReport,
    CreateRequest, OutputPolicy, Recovery, SliceStrategy,
};
use output::StagedOutputs;

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
    plan::validate_request(&request)?;

    let mut input_files = plan::collect_files(request.input_paths(), request.recurses())?;
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

        let ingest::PassResult {
            recovery_slices,
            slice_checksums,
            hashes,
        } = ingest::encode_pass(
            ingest::PassRequest {
                input_files: &input_files,
                slice_size,
                total_slices,
                first_exponent,
                recovery_blocks: count,
                pass,
                input_bytes,
                memory_plan,
                memory_limit: options.memory_limit,
                layout: engine.layout,
                simd: options.simd,
                thread_pool: Arc::clone(&thread_pool),
            },
            cancellation,
            &mut on_event,
        )
        .await?;

        if pass == 0 {
            (recovery_set_id, base_packets) = packet_output::build_base_packets(
                &input_files,
                slice_size,
                &mut all_checksums,
                slice_checksums,
                hashes,
                &creator,
            );

            if engine.write_index {
                check_cancellation(cancellation, &mut on_event)?;
                packet_output::write_index(&outputs, &base_packets)?;
            }
        }

        packet_output::append_recovery_packets(
            packet_output::RecoveryOutput {
                outputs: &outputs,
                recovery_count,
                recovery_offset: request.requested_recovery_offset(),
                recovery_set_id: &recovery_set_id,
                base_packets: &base_packets,
                recovery_slices: &recovery_slices,
            },
            cancellation,
            &mut on_event,
        )
        .await?;

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

pub(super) fn check_cancellation<F>(
    cancellation: Option<&CreateCancellation>,
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(CreateEvent),
{
    if cancellation.is_some_and(CreateCancellation::is_cancelled) {
        on_event(CreateEvent::Cancelled);
        return Err(anyhow::Error::new(CancelledFailure));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
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
