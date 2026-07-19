//! PAR2 quick-check: derives a file's expected whole-file CRC-32 purely
//! from its PAR2 IFSC slice checksums, and compares it against the CRC-32
//! [`crate::assemble::assemble`] already computed while writing — no
//! second read of the file's bytes at all. Inspired by `sabnzbd`'s
//! `par2file.parse_par2_file` (`ROADMAP.md` Phase 16), reimplemented here
//! using only a *forward* CRC-32-combine primitive
//! ([`pesto::yenc::crc32_combine`]) rather than `sabnzbd`'s extra "undo
//! zero padding" primitive (`sabctools.crc32_zero_unpad`): instead of
//! removing the last slice's zero padding from its PAR2-supplied CRC-32,
//! this pads the *real*, already-known file CRC-32 forward to the same
//! slice boundary and compares like for like — avoiding any need to
//! invert the combine operator at all.
//!
//! Per the PAR2 spec, every slice except a file's last is exactly
//! `slice_size` bytes; the last is zero-padded up to `slice_size` before
//! its IFSC checksum is computed. That means every slice's IFSC CRC-32 —
//! including the last — is genuinely `crc32(exactly slice_size bytes)`,
//! so folding [`pesto::yenc::crc32_combine`] over every slice in order,
//! always with `len2 = slice_size`, reconstructs the CRC-32 PAR2 expects
//! for the whole file *padded* to a multiple of `slice_size`.
//!
//! Only ever used to *skip* the expensive [`pesto::par2::verify::verify`]
//! pass in the all-intact common case — never to decide *which* files are
//! damaged (that still needs the byte-exact, slice-by-slice comparison
//! `verify()` performs). See [`crate::repair`] for how the two are
//! composed.
//!
//! **Scope, honestly stated:** this only re-validates the CRC-32
//! [`crate::assemble::assemble`] computed *at write time*, from the
//! decoded segments as fetched — it never re-reads the file as it
//! currently sits on disk. A download that assembled correctly but was
//! silently corrupted afterward by something outside `penne` (disk
//! bitrot, a stray process) would pass this quick-check yet fail a real
//! `verify()`. That is an intentional trade-off, not an oversight: this
//! module answers "did the download itself succeed", the question
//! `penne download` actually needs answered after every run; a user
//! specifically worried about at-rest disk corruption already has
//! `pesto::par2::verify::verify` (or a filesystem with its own
//! checksumming) for that, and can still reach it — nothing here removes
//! the ability to run a full verify, it only avoids paying for one
//! unconditionally on every single successful run.

use pesto::par2::recovery_set::FileEntry;
use pesto::yenc::crc32_combine;

/// Fold a file's PAR2 slice checksums into the CRC-32 PAR2 expects for
/// that file, padded to a multiple of `slice_size` — see the module doc
/// comment for why every slice (including the last) is combined with the
/// same `len2 = slice_size`. `None` when `file` carries no slice checksums
/// at all (no IFSC packet was found for it — nothing to quick-check
/// against).
fn expected_padded_crc32(file: &FileEntry, slice_size: u64) -> Option<u32> {
    if file.slice_checksums.is_empty() {
        return None;
    }
    let mut acc = 0u32;
    for slice in &file.slice_checksums {
        acc = crc32_combine(acc, slice.crc32, slice_size);
    }
    Some(acc)
}

/// Pad `real_crc32` (the CRC-32 of `real_len` real bytes) forward to a
/// multiple of `slice_size`, matching what PAR2's own zero-padded last
/// slice would hash to. A no-op (`real_crc32` unchanged) when `real_len`
/// is already a multiple of `slice_size`.
fn pad_crc32_to_slice_boundary(real_crc32: u32, real_len: u64, slice_size: u64) -> u32 {
    if slice_size == 0 {
        return real_crc32;
    }
    let remainder = real_len % slice_size;
    if remainder == 0 {
        return real_crc32;
    }
    let pad_len = slice_size - remainder;
    let pad_crc32 = pesto::yenc::crc32(&vec![0u8; pad_len as usize]);
    crc32_combine(real_crc32, pad_crc32, pad_len)
}

/// Compare `real_crc32` (a file's already-known, real whole-file CRC-32,
/// as [`crate::assemble::assemble`] computed it) against what `file`'s
/// PAR2 IFSC data implies it should be. `file.length` (from the PAR2 File
/// Description packet) is used as the padding boundary — a file whose
/// real length doesn't actually match `file.length` (wrong match, or
/// truncation) will, with overwhelming probability, simply fail this
/// comparison rather than need special-casing, since padding to the wrong
/// boundary practically never produces a colliding CRC-32.
///
/// `Some(true)`/`Some(false)` when `file` has slice checksums to compare
/// against; `None` when it doesn't (no IFSC packet — quick check
/// inconclusive, caller must fall back to a full, byte-exact verify).
pub fn looks_intact(file: &FileEntry, slice_size: u64, real_crc32: u32) -> Option<bool> {
    let expected = expected_padded_crc32(file, slice_size)?;
    let padded_real = pad_crc32_to_slice_boundary(real_crc32, file.length, slice_size);
    Some(padded_real == expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pesto::par2::packet::SliceChecksum;

    fn file_entry(length: u64, slice_checksums: Vec<SliceChecksum>) -> FileEntry {
        FileEntry {
            file_id: [0; 16],
            name: "f.bin".to_string(),
            length,
            md5_full: [0; 16],
            md5_16k: [0; 16],
            slice_checksums,
        }
    }

    fn slice(crc32: u32) -> SliceChecksum {
        SliceChecksum {
            md5: [0; 16],
            crc32,
        }
    }

    #[test]
    fn no_slice_checksums_is_inconclusive() {
        let file = file_entry(100, vec![]);
        assert_eq!(looks_intact(&file, 128, 0), None);
    }

    #[test]
    fn matches_a_hand_combined_single_full_slice() {
        // A file exactly one slice long: PAR2 pads nothing, so the
        // expected padded CRC-32 is simply the slice's own CRC-32, and the
        // real CRC-32 needs no padding either (length is already a
        // multiple of slice_size).
        let data = b"exactly sixteen!"; // 16 bytes
        let slice_size = 16u64;
        let real_crc = pesto::yenc::crc32(data);
        let file = file_entry(data.len() as u64, vec![slice(real_crc)]);
        assert_eq!(looks_intact(&file, slice_size, real_crc), Some(true));
    }

    #[test]
    fn detects_a_mismatch() {
        let file = file_entry(16, vec![slice(0xDEAD_BEEF)]);
        assert_eq!(looks_intact(&file, 16, 0x1234_5678), Some(false));
    }

    #[test]
    fn pad_to_slice_boundary_is_a_no_op_when_already_aligned() {
        assert_eq!(pad_crc32_to_slice_boundary(0xABCD, 32, 16), 0xABCD);
    }

    #[test]
    fn pad_to_slice_boundary_matches_actually_padded_data() {
        let real = b"short tail";
        let slice_size = 16u64;
        let padded_crc =
            pad_crc32_to_slice_boundary(pesto::yenc::crc32(real), real.len() as u64, slice_size);

        let mut padded = real.to_vec();
        padded.resize(slice_size as usize, 0);
        assert_eq!(padded_crc, pesto::yenc::crc32(&padded));
    }
}
