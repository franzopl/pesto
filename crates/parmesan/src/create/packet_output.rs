use super::{check_cancellation, CreateCancellation, CreateEvent};
use crate::encoder::{FileHashes, RecoverySlice};
use crate::ops::InputFile;
use crate::{layout, packet};
use anyhow::{Context, Result};
use md5::{Digest, Md5};
use tokio::io::AsyncWriteExt;

use super::output::StagedOutputs;

pub(super) fn build_base_packets(
    input_files: &[InputFile],
    slice_size: usize,
    all_checksums: &mut [Vec<packet::SliceChecksum>],
    slice_checksums: Vec<packet::SliceChecksum>,
    hashes: Vec<FileHashes>,
    creator: &str,
) -> ([u8; 16], Vec<u8>) {
    // Empty files have no input slices, so the worker returns no hash for
    // them. They still need File Description entries.
    let md5_empty: [u8; 16] = Md5::digest([]).into();
    let mut worker_hashes = hashes.into_iter();
    let all_hashes: Vec<FileHashes> = input_files
        .iter()
        .map(|file| {
            if file.size == 0 {
                FileHashes {
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
            packet::compute_file_id(&all_hashes[index].md5_16k, file.size, &file.display_name)
        })
        .collect();
    let main_body = packet::main_body(slice_size as u64, &file_ids);
    let recovery_set_id = packet::recovery_set_id(&main_body);
    let mut packets = packet::serialize_packet(&recovery_set_id, &packet::TYPE_MAIN, &main_body);
    packets.extend(packet::serialize_packet(
        &recovery_set_id,
        &packet::TYPE_CREATOR,
        &packet::creator_body(creator),
    ));
    for (index, file) in input_files.iter().enumerate() {
        let file_id = &file_ids[index];
        packets.extend(packet::serialize_packet(
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
        packets.extend(packet::serialize_packet(
            &recovery_set_id,
            &packet::TYPE_IFSC,
            &packet::ifsc_body(file_id, &all_checksums[index]),
        ));
    }
    (recovery_set_id, packets)
}

pub(super) fn write_index(outputs: &StagedOutputs, base_packets: &[u8]) -> Result<()> {
    let path = outputs
        .staged_index_path()
        .expect("index path exists when index output is enabled");
    std::fs::write(path, base_packets).with_context(|| format!("writing `{}`", path.display()))
}

pub(super) struct RecoveryOutput<'a> {
    pub outputs: &'a StagedOutputs,
    pub recovery_count: usize,
    pub recovery_offset: u32,
    pub recovery_set_id: &'a [u8; 16],
    pub base_packets: &'a [u8],
    pub recovery_slices: &'a [RecoverySlice],
}

pub(super) async fn append_recovery_packets<F>(
    request: RecoveryOutput<'_>,
    cancellation: Option<&CreateCancellation>,
    on_event: &mut F,
) -> Result<()>
where
    F: FnMut(CreateEvent),
{
    let volumes = layout::plan_volumes(request.recovery_count as u32);
    let packets =
        packet::serialize_recovery_packets(request.recovery_set_id, request.recovery_slices);
    for (slice, packet) in request.recovery_slices.iter().zip(packets) {
        check_cancellation(cancellation, on_event)?;
        let layout_exponent = slice.exponent - request.recovery_offset;
        let volume = volumes
            .iter()
            .find(|volume| {
                layout_exponent >= volume.first && layout_exponent < volume.first + volume.count
            })
            .expect("volume layout covers every recovery block");
        let path = request.outputs.staged_volume_path(*volume);

        if layout_exponent == volume.first {
            tokio::fs::write(path, request.base_packets).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .await?;
        file.write_all(&packet).await?;
    }
    Ok(())
}
