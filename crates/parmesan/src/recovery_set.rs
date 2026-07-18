//! Assembles a [`RecoverySet`] by reading an index file and its recovery
//! volumes from disk.
//!
//! Per the PAR2 spec, the global order of input (source) blocks used for
//! Reed-Solomon coefficients is the *numeric order of File IDs as listed in
//! the Main packet*, not the order files happen to appear on disk or on the
//! command line. [`RecoverySet::files`] always reflects that canonical
//! order. Note that as of this writing the encoder in [`crate::ops`] /
//! `main.rs` feeds slices to [`crate::encoder::RecoveryEncoder`] in
//! command-line order instead — see the Phase 22 notes in `ROADMAP.md` for
//! why that must be reconciled before repair (Phase 22d) can be correct
//! against third-party PAR2 tools. Verification does not depend on
//! Reed-Solomon coefficients at all, so it is unaffected.

use crate::packet::{self, SliceChecksum};
use crate::packet_reader::{read_packets, RawPacket};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Fields parsed from one File Description packet.
struct FileDescFields {
    file_id: [u8; 16],
    name: String,
    md5_full: [u8; 16],
    md5_16k: [u8; 16],
    length: u64,
}

/// One file described by the recovery set.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// PAR2 File ID: `md5(md5_16k || length_le || name)`.
    pub file_id: [u8; 16],
    /// File name as stored in the File Description packet.
    pub name: String,
    /// Full file length in bytes.
    pub length: u64,
    /// MD5 of the whole file.
    pub md5_full: [u8; 16],
    /// MD5 of the first 16 KiB of the file.
    pub md5_16k: [u8; 16],
    /// Per-slice MD5 + CRC32 checksums, in slice order, from the IFSC
    /// packet. Empty if no IFSC packet was found for this file.
    pub slice_checksums: Vec<SliceChecksum>,
}

/// A fully assembled PAR2 recovery set: the Main packet's file list plus
/// every recovery block found across the index and volume files on disk.
#[derive(Debug, Clone)]
pub struct RecoverySet {
    /// Recovery set ID (MD5 of the Main packet body).
    pub recovery_set_id: [u8; 16],
    /// Slice size in bytes, shared by every file in the set.
    pub slice_size: u64,
    /// Files in the canonical order used for Reed-Solomon coefficients
    /// (numeric order of File ID, per the PAR2 spec).
    pub files: Vec<FileEntry>,
    /// Recovery blocks found on disk, keyed by exponent.
    pub recovery_blocks: BTreeMap<u32, Vec<u8>>,
}

impl RecoverySet {
    /// Load a recovery set starting from its index file, scanning the same
    /// directory for every `.par2` file that belongs to the same recovery
    /// set — matched by recovery-set ID, not by file name, so any naming
    /// scheme (this encoder's or another tool's) is picked up.
    pub fn load(index_path: impl AsRef<Path>) -> Result<Self> {
        let index_path = index_path.as_ref();
        let dir = index_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        let index_bytes = std::fs::read(index_path)
            .with_context(|| format!("reading index file `{}`", index_path.display()))?;
        let index_packets = read_packets(&index_bytes);

        let main_raw = index_packets
            .iter()
            .find(|p| p.packet_type == packet::TYPE_MAIN)
            .with_context(|| format!("no Main packet found in `{}`", index_path.display()))?;
        let recovery_set_id = main_raw.recovery_set_id;
        let (slice_size, recovery_file_ids) = parse_main_body(&main_raw.body)?;

        let mut all_packets: Vec<RawPacket> = Vec::new();
        for entry in std::fs::read_dir(dir)
            .with_context(|| format!("reading directory `{}`", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let is_par2 = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("par2"))
                .unwrap_or(false);
            if !is_par2 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue; // unreadable file (permissions, race) — skip, not fatal
            };
            all_packets.extend(
                read_packets(&bytes)
                    .into_iter()
                    .filter(|p| p.recovery_set_id == recovery_set_id),
            );
        }

        dedup_packets(&mut all_packets);

        let mut file_desc: BTreeMap<[u8; 16], FileDescFields> = BTreeMap::new();
        let mut ifsc: BTreeMap<[u8; 16], Vec<SliceChecksum>> = BTreeMap::new();
        let mut recovery_blocks: BTreeMap<u32, Vec<u8>> = BTreeMap::new();

        for p in &all_packets {
            if p.packet_type == packet::TYPE_FILE_DESC {
                if let Ok(fields) = parse_file_desc_body(&p.body) {
                    file_desc.insert(fields.file_id, fields);
                }
            } else if p.packet_type == packet::TYPE_IFSC {
                if let Ok((fid, slices)) = parse_ifsc_body(&p.body) {
                    ifsc.insert(fid, slices);
                }
            } else if p.packet_type == packet::TYPE_RECOVERY {
                if let Ok((exponent, data)) = parse_recovery_body(&p.body) {
                    recovery_blocks.entry(exponent).or_insert(data);
                }
            }
        }

        let mut files = Vec::with_capacity(recovery_file_ids.len());
        for fid in &recovery_file_ids {
            let fields = file_desc.get(fid).with_context(|| {
                "recovery set references a File ID with no matching File Description packet"
            })?;
            let slice_checksums = ifsc.get(fid).cloned().unwrap_or_default();
            files.push(FileEntry {
                file_id: fields.file_id,
                name: fields.name.clone(),
                length: fields.length,
                md5_full: fields.md5_full,
                md5_16k: fields.md5_16k,
                slice_checksums,
            });
        }

        Ok(Self {
            recovery_set_id,
            slice_size,
            files,
            recovery_blocks,
        })
    }
}

/// Drop duplicate packets — the same packet often appears in more than one
/// volume file — keeping the first occurrence. Keyed by a hash of the body
/// rather than the body itself so large Recovery Slice packets aren't
/// cloned just to check membership.
fn dedup_packets(packets: &mut Vec<RawPacket>) {
    let mut seen: std::collections::HashSet<([u8; 16], [u8; 16])> =
        std::collections::HashSet::new();
    packets.retain(|p| seen.insert((p.packet_type, packet::md5(&p.body))));
}

fn parse_main_body(body: &[u8]) -> Result<(u64, Vec<[u8; 16]>)> {
    anyhow::ensure!(body.len() >= 12, "Main packet body too short");
    let slice_size = u64::from_le_bytes(body[0..8].try_into().unwrap());
    let count = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
    let need = 12 + count * 16;
    anyhow::ensure!(
        body.len() >= need,
        "Main packet body truncated: expected at least {need} bytes, got {}",
        body.len()
    );
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let off = 12 + i * 16;
        let mut id = [0u8; 16];
        id.copy_from_slice(&body[off..off + 16]);
        ids.push(id);
    }
    Ok((slice_size, ids))
}

fn parse_file_desc_body(body: &[u8]) -> Result<FileDescFields> {
    anyhow::ensure!(body.len() >= 56, "File Description packet body too short");
    let mut file_id = [0u8; 16];
    file_id.copy_from_slice(&body[0..16]);
    let mut md5_full = [0u8; 16];
    md5_full.copy_from_slice(&body[16..32]);
    let mut md5_16k = [0u8; 16];
    md5_16k.copy_from_slice(&body[32..48]);
    let length = u64::from_le_bytes(body[48..56].try_into().unwrap());
    let raw_name = &body[56..];
    // Names are zero-padded to a multiple of 4 bytes on write; trim that
    // padding back off (file names never legitimately end in NUL bytes).
    let end = raw_name
        .iter()
        .rposition(|&b| b != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let name = String::from_utf8_lossy(&raw_name[..end]).into_owned();
    Ok(FileDescFields {
        file_id,
        name,
        md5_full,
        md5_16k,
        length,
    })
}

fn parse_ifsc_body(body: &[u8]) -> Result<([u8; 16], Vec<SliceChecksum>)> {
    anyhow::ensure!(body.len() >= 16, "IFSC packet body too short");
    let mut file_id = [0u8; 16];
    file_id.copy_from_slice(&body[0..16]);
    let rest = &body[16..];
    anyhow::ensure!(
        rest.len().is_multiple_of(20),
        "IFSC packet body has a partial slice-checksum entry"
    );
    let mut slices = Vec::with_capacity(rest.len() / 20);
    for chunk in rest.chunks_exact(20) {
        let mut md5 = [0u8; 16];
        md5.copy_from_slice(&chunk[0..16]);
        let crc32 = u32::from_le_bytes(chunk[16..20].try_into().unwrap());
        slices.push(SliceChecksum { md5, crc32 });
    }
    Ok((file_id, slices))
}

fn parse_recovery_body(body: &[u8]) -> Result<(u32, Vec<u8>)> {
    anyhow::ensure!(body.len() >= 4, "Recovery Slice packet body too short");
    let exponent = u32::from_le_bytes(body[0..4].try_into().unwrap());
    Ok((exponent, body[4..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{build_fixture_set, FixtureFile};

    #[test]
    fn loads_files_in_ascending_file_id_order() {
        let (dir, index) = build_fixture_set(
            "recovery-set-order",
            &[
                FixtureFile {
                    name: "a.bin",
                    data: vec![1u8; 300],
                },
                FixtureFile {
                    name: "b.bin",
                    data: vec![2u8; 500],
                },
            ],
            128,
            4,
        );

        let set = RecoverySet::load(&index).unwrap();
        assert_eq!(set.files.len(), 2);
        assert!(set.files[0].file_id <= set.files[1].file_id);
        let names: Vec<&str> = set.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"a.bin"));
        assert!(names.contains(&"b.bin"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loads_recovery_blocks_and_slice_checksums() {
        let (dir, index) = build_fixture_set(
            "recovery-set-blocks",
            &[FixtureFile {
                name: "only.bin",
                data: vec![7u8; 1000],
            }],
            128,
            5,
        );

        let set = RecoverySet::load(&index).unwrap();
        assert_eq!(set.recovery_blocks.len(), 5);
        assert_eq!(set.files.len(), 1);
        let expected_slices = 1000usize.div_ceil(128);
        assert_eq!(set.files[0].slice_checksums.len(), expected_slices);
        assert_eq!(set.slice_size, 128);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_index_file_is_a_clean_error() {
        let result = RecoverySet::load("/nonexistent/path/movie.mkv.par2");
        assert!(result.is_err());
    }
}
