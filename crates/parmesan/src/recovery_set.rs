//! Assembles a [`RecoverySet`] by reading an index file and its recovery
//! volumes from disk.
//!
//! Per the PAR2 spec, the global order of input (source) blocks used for
//! Reed-Solomon coefficients is the *numeric order of File IDs as listed in
//! the Main packet*, not the order files happen to appear on disk or on the
//! command line. [`RecoverySet::files`] always reflects that canonical
//! order. The encoder in [`crate::ops`] sorts the same way (`sort_files_by_file_id`)
//! before feeding slices to [`crate::encoder::RecoveryEncoder`], so encode,
//! verify and repair share one File-ID order.

use crate::packet::{self, SliceChecksum};
use crate::packet_reader::{read_packets, read_packets_with_offsets};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

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

/// On-disk location of one recovery block. The body is not held in RAM
/// until [`RecoverySet::load_recovery_blocks`] reads it.
#[derive(Debug, Clone)]
pub struct RecoveryBlockLoc {
    pub path: PathBuf,
    /// Byte offset of the recovery *data* (after the 4-byte exponent).
    pub offset: u64,
    pub len: usize,
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
    /// Recovery blocks loaded into memory, keyed by exponent. Empty after
    /// [`Self::load_metadata`] until [`Self::load_recovery_blocks`].
    pub recovery_blocks: BTreeMap<u32, Vec<u8>>,
    /// Every recovery block on disk, whether or not its body is loaded.
    pub recovery_index: BTreeMap<u32, RecoveryBlockLoc>,
}

impl RecoverySet {
    /// Load a recovery set starting from its index file, scanning the same
    /// directory for every `.par2` file that belongs to the same recovery
    /// set — matched by recovery-set ID, not by file name, so any naming
    /// scheme (this encoder's or another tool's) is picked up.
    ///
    /// This loads every recovery block into RAM. Prefer
    /// [`Self::load_metadata`] when the caller only needs the file list
    /// (`verify`, `health`, deobfuscation).
    pub fn load(index_path: impl AsRef<Path>) -> Result<Self> {
        let mut set = Self::load_metadata(index_path)?;
        set.load_recovery_blocks(None)?;
        Ok(set)
    }

    /// Load Main / File Description / IFSC metadata and index recovery
    /// blocks by `(path, offset, len)` without retaining their bodies.
    pub fn load_metadata(index_path: impl AsRef<Path>) -> Result<Self> {
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

        let mut file_desc: BTreeMap<[u8; 16], FileDescFields> = BTreeMap::new();
        let mut ifsc: BTreeMap<[u8; 16], Vec<SliceChecksum>> = BTreeMap::new();
        let mut recovery_index: BTreeMap<u32, RecoveryBlockLoc> = BTreeMap::new();
        let mut seen_packets: std::collections::HashSet<([u8; 16], [u8; 16])> =
            std::collections::HashSet::new();

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
            for (p, offset) in read_packets_with_offsets(&bytes) {
                if p.recovery_set_id != recovery_set_id {
                    continue;
                }
                if !seen_packets.insert((p.packet_type, packet::md5(&p.body))) {
                    continue;
                }
                if p.packet_type == packet::TYPE_FILE_DESC {
                    if let Ok(fields) = parse_file_desc_body(&p.body) {
                        file_desc.insert(fields.file_id, fields);
                    }
                } else if p.packet_type == packet::TYPE_IFSC {
                    if let Ok((fid, slices)) = parse_ifsc_body(&p.body) {
                        ifsc.insert(fid, slices);
                    }
                } else if p.packet_type == packet::TYPE_RECOVERY && p.body.len() >= 4 {
                    let exponent = u32::from_le_bytes(p.body[0..4].try_into().unwrap());
                    let data_len = p.body.len() - 4;
                    recovery_index.entry(exponent).or_insert(RecoveryBlockLoc {
                        path: path.clone(),
                        offset: (offset + packet::HEADER_LEN + 4) as u64,
                        len: data_len,
                    });
                }
            }
        }

        let mut files = Vec::with_capacity(recovery_file_ids.len());
        for fid in &recovery_file_ids {
            let fields = file_desc.get(fid).with_context(|| {
                "recovery set references a File ID with no matching File Description packet"
            })?;
            let name = sanitize_par2_name(&fields.name)?;
            let slice_checksums = ifsc.get(fid).cloned().unwrap_or_default();
            if !slice_checksums.is_empty() && slice_size > 0 {
                let expected = fields.length.div_ceil(slice_size);
                if slice_checksums.len() as u64 != expected {
                    bail!(
                        "PAR2 File Description for `{name}` has {} IFSC slices, expected {expected} \
                         (length {} / slice size {slice_size})",
                        slice_checksums.len(),
                        fields.length
                    );
                }
            }
            files.push(FileEntry {
                file_id: fields.file_id,
                name,
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
            recovery_blocks: BTreeMap::new(),
            recovery_index,
        })
    }

    /// How many distinct recovery blocks exist on disk (whether loaded or not).
    pub fn available_recovery_blocks(&self) -> usize {
        self.recovery_index.len().max(self.recovery_blocks.len())
    }

    /// Read recovery-block bodies into [`Self::recovery_blocks`].
    ///
    /// `max_blocks` limits how many are loaded (lowest exponents first).
    /// Reed-Solomon over GF(2¹⁶) is MDS, so any `m` blocks reconstruct `m`
    /// missing inputs — repair only needs `total_bad_slices()` of them.
    pub fn load_recovery_blocks(&mut self, max_blocks: Option<usize>) -> Result<()> {
        let limit = max_blocks.unwrap_or(self.recovery_index.len());
        for (exponent, loc) in self.recovery_index.iter().take(limit) {
            if self.recovery_blocks.contains_key(exponent) {
                continue;
            }
            let mut file = std::fs::File::open(&loc.path).with_context(|| {
                format!("opening `{}` to read a recovery block", loc.path.display())
            })?;
            use std::io::{Read, Seek, SeekFrom};
            file.seek(SeekFrom::Start(loc.offset))?;
            let mut data = vec![0u8; loc.len];
            file.read_exact(&mut data).with_context(|| {
                format!(
                    "reading recovery block exponent {exponent} from `{}`",
                    loc.path.display()
                )
            })?;
            self.recovery_blocks.insert(*exponent, data);
        }
        Ok(())
    }
}

/// Sanitize a PAR2 File Description name before it is used as a path.
///
/// Rejects empty names, absolute paths, Windows drive/UNC prefixes and any
/// `..` component. Legitimate relative subdirectories are kept (`dir/file.bin`)
/// so a recovery set that stored a tree can still be verified and repaired
/// under `base`. Backslashes are treated as separators.
pub fn sanitize_par2_name(name: &str) -> Result<String> {
    let normalized = name.replace('\\', "/");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        bail!("PAR2 file name is empty");
    }
    let bytes = trimmed.as_bytes();
    if trimmed.starts_with('/') {
        bail!("PAR2 file name `{name}` is an absolute path");
    }
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        bail!("PAR2 file name `{name}` has a drive prefix");
    }
    let mut parts = Vec::new();
    for comp in trimmed.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            bail!("PAR2 file name `{name}` contains a `..` component");
        }
        parts.push(comp);
    }
    if parts.is_empty() {
        bail!("PAR2 file name `{name}` has no usable path components");
    }
    Ok(parts.join("/"))
}

/// Join `name` onto `base` and assert the result stays under `base`.
///
/// `name` must already have been through [`sanitize_par2_name`]. The extra
/// check is belt-and-braces against a future caller that skips sanitization.
pub fn contained_path(base: &Path, name: &str) -> Result<PathBuf> {
    let joined = base.join(name);
    if !path_is_under(base, &joined) {
        bail!(
            "PAR2 file name `{name}` escapes base directory `{}`",
            base.display()
        );
    }
    Ok(joined)
}

fn path_is_under(base: &Path, candidate: &Path) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let abs = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    fn normalize(p: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out
    }
    normalize(&abs(candidate)).starts_with(normalize(&abs(base)))
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
    for chunk in rest.as_chunks::<20>().0 {
        let mut md5 = [0u8; 16];
        md5.copy_from_slice(&chunk[0..16]);
        let crc32 = u32::from_le_bytes(chunk[16..20].try_into().unwrap());
        slices.push(SliceChecksum { md5, crc32 });
    }
    Ok((file_id, slices))
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
        assert_eq!(set.recovery_index.len(), 5);
        assert_eq!(set.files.len(), 1);
        let expected_slices = 1000usize.div_ceil(128);
        assert_eq!(set.files[0].slice_checksums.len(), expected_slices);
        assert_eq!(set.slice_size, 128);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_metadata_indexes_blocks_without_holding_them() {
        let (dir, index) = build_fixture_set(
            "recovery-set-meta",
            &[FixtureFile {
                name: "only.bin",
                data: vec![7u8; 1000],
            }],
            128,
            5,
        );

        let mut set = RecoverySet::load_metadata(&index).unwrap();
        assert_eq!(set.recovery_index.len(), 5);
        assert!(set.recovery_blocks.is_empty());
        assert_eq!(set.available_recovery_blocks(), 5);

        set.load_recovery_blocks(Some(2)).unwrap();
        assert_eq!(set.recovery_blocks.len(), 2);
        set.load_recovery_blocks(None).unwrap();
        assert_eq!(set.recovery_blocks.len(), 5);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_index_file_is_a_clean_error() {
        let result = RecoverySet::load("/nonexistent/path/movie.mkv.par2");
        assert!(result.is_err());
    }

    #[test]
    fn sanitize_par2_name_keeps_relative_subdirectories() {
        assert_eq!(
            sanitize_par2_name("extras/behind.bin").unwrap(),
            "extras/behind.bin"
        );
        assert_eq!(
            sanitize_par2_name(r"extras\behind.bin").unwrap(),
            "extras/behind.bin"
        );
    }

    #[test]
    fn sanitize_par2_name_rejects_traversal_and_absolute() {
        assert!(sanitize_par2_name("../secret").is_err());
        assert!(sanitize_par2_name("/etc/passwd").is_err());
        assert!(sanitize_par2_name("C:\\windows\\system32").is_err());
        assert!(sanitize_par2_name("foo/../../etc/passwd").is_err());
        assert!(sanitize_par2_name("").is_err());
    }

    #[test]
    fn contained_path_stays_under_base() {
        let base = Path::new("/tmp/release");
        let p = contained_path(base, "a.bin").unwrap();
        assert_eq!(p, Path::new("/tmp/release/a.bin"));
        assert!(contained_path(base, "../escape.bin").is_err());
    }
}
