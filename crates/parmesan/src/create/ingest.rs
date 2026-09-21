use super::{check_cancellation, CreateCancellation, CreateEvent};
use crate::encoder::{FileHashes, RecoveryEncoder, RecoverySlice};
use crate::ops::{
    ingest_files_ex, ingest_files_with_progress, IngestHashes, InputFile, MemoryLayout, SliceWindow,
};
use crate::packet::SliceChecksum;
use crate::{EncoderLayout, SimdPath};
use anyhow::Result;
use std::sync::Arc;

pub(super) struct PassRequest<'a> {
    pub input_files: &'a [InputFile],
    pub slice_size: usize,
    pub total_slices: usize,
    pub first_exponent: u32,
    pub recovery_blocks: usize,
    pub pass: usize,
    pub input_bytes: u64,
    pub memory_plan: MemoryLayout,
    pub memory_limit: usize,
    pub layout: EncoderLayout,
    pub simd: SimdPath,
    pub thread_pool: Arc<rayon::ThreadPool>,
}

pub(super) struct PassResult {
    pub recovery_slices: Vec<RecoverySlice>,
    pub slice_checksums: Vec<SliceChecksum>,
    pub hashes: Vec<FileHashes>,
}

pub(super) async fn encode_pass<F>(
    request: PassRequest<'_>,
    cancellation: Option<&CreateCancellation>,
    on_event: &mut F,
) -> Result<PassResult>
where
    F: FnMut(CreateEvent),
{
    if request.memory_plan.slice_chunk < request.slice_size {
        encode_chunked(request, cancellation, on_event).await
    } else {
        encode_full_slices(request, cancellation, on_event).await
    }
}

async fn encode_chunked<F>(
    request: PassRequest<'_>,
    cancellation: Option<&CreateCancellation>,
    on_event: &mut F,
) -> Result<PassResult>
where
    F: FnMut(CreateEvent),
{
    let mut ingest_hashes = IngestHashes::default();
    let mut accumulated: Vec<Vec<u8>> = vec![Vec::new(); request.recovery_blocks];
    let mut offset = 0usize;
    let mut bytes_read = 0u64;
    while offset < request.slice_size {
        check_cancellation(cancellation, on_event)?;
        let len = request
            .memory_plan
            .slice_chunk
            .min(request.slice_size - offset);
        let encoder = configured_encoder(&request, len);
        let worker = crate::worker::Par2Worker::spawn_with_thread_pool(
            encoder,
            false,
            crate::worker::DEFAULT_CHANNEL_DEPTH,
            Arc::clone(&request.thread_pool),
        );
        let hash_slot = (request.pass == 0 && offset == 0).then_some(&mut ingest_hashes);
        ingest_files_ex(
            request.input_files,
            &worker,
            request.slice_size,
            cancellation.map(CreateCancellation::as_atomic),
            |_| Ok(()),
            Some(SliceWindow { offset, len }),
            hash_slot,
        )
        .await?;
        bytes_read += request.input_bytes;
        on_event(CreateEvent::BytesRead {
            pass: request.pass,
            bytes_read,
        });
        let (part, _, _) = tokio::task::block_in_place(|| worker.finish());
        check_cancellation(cancellation, on_event)?;
        for (index, slice) in part.into_iter().enumerate() {
            accumulated[index].extend_from_slice(&slice.data);
        }
        offset += len;
    }

    let recovery_slices = accumulated
        .into_iter()
        .enumerate()
        .map(|(index, data)| RecoverySlice {
            exponent: request.first_exponent + index as u32,
            data,
        })
        .collect();
    Ok(PassResult {
        recovery_slices,
        slice_checksums: ingest_hashes.checksums,
        hashes: ingest_hashes.hashes,
    })
}

async fn encode_full_slices<F>(
    request: PassRequest<'_>,
    cancellation: Option<&CreateCancellation>,
    on_event: &mut F,
) -> Result<PassResult>
where
    F: FnMut(CreateEvent),
{
    let mut encoder = configured_encoder(&request, request.slice_size);
    if request.pass == 0 {
        encoder = encoder.with_checksums();
    }
    let worker = crate::worker::Par2Worker::spawn_with_thread_pool(
        encoder,
        request.pass == 0,
        crate::worker::DEFAULT_CHANNEL_DEPTH,
        Arc::clone(&request.thread_pool),
    );
    let mut bytes_read = 0u64;
    ingest_files_with_progress(
        request.input_files,
        &worker,
        request.slice_size,
        cancellation.map(CreateCancellation::as_atomic),
        |_| Ok(()),
        |read| {
            bytes_read += read as u64;
            on_event(CreateEvent::BytesRead {
                pass: request.pass,
                bytes_read,
            });
            Ok(())
        },
    )
    .await?;
    let (recovery_slices, slice_checksums, hashes) =
        tokio::task::block_in_place(|| worker.finish());
    check_cancellation(cancellation, on_event)?;
    Ok(PassResult {
        recovery_slices,
        slice_checksums,
        hashes,
    })
}

fn configured_encoder(request: &PassRequest<'_>, slice_size: usize) -> RecoveryEncoder {
    let encoder = match request.layout {
        EncoderLayout::Smart => RecoveryEncoder::new_smart(
            slice_size,
            request.total_slices,
            request.first_exponent,
            request.recovery_blocks,
        ),
        EncoderLayout::Normal => RecoveryEncoder::new(
            slice_size,
            request.total_slices,
            request.first_exponent,
            request.recovery_blocks,
        ),
        EncoderLayout::Affine => RecoveryEncoder::new_affine(
            slice_size,
            request.total_slices,
            request.first_exponent,
            request.recovery_blocks,
        ),
        EncoderLayout::Affine512 => RecoveryEncoder::new_affine512(
            slice_size,
            request.total_slices,
            request.first_exponent,
            request.recovery_blocks,
        ),
        EncoderLayout::Shuffle2x => RecoveryEncoder::new_shuffle2x(
            slice_size,
            request.total_slices,
            request.first_exponent,
            request.recovery_blocks,
        ),
    };
    encoder
        .with_simd_path(request.simd)
        .with_flush_limit((request.memory_limit / 4).clamp(256 * 1024 * 1024, 1024 * 1024 * 1024))
}
