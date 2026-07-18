//! PAR2 packet serialization.
//!
//! Every PAR2 packet is a 64-byte header followed by a body whose length is a
//! multiple of 4. The header is:
//!
//! | offset | size | field                                            |
//! |--------|------|--------------------------------------------------|
//! | 0      | 8    | magic `PAR2\0PKT`                                |
//! | 8      | 8    | total packet length (u64 LE)                     |
//! | 16     | 16   | MD5 of the packet from offset 32 onward          |
//! | 32     | 16   | recovery set ID                                  |
//! | 48     | 16   | packet type                                      |
//!
//! All integers are little-endian.

use md5::{Digest, Md5};

pub(crate) const MAGIC: [u8; 8] = *b"PAR2\0PKT";

/// Size of the fixed packet header.
pub const HEADER_LEN: usize = 64;

/// Packet type tag: Main packet.
pub const TYPE_MAIN: [u8; 16] = *b"PAR 2.0\0Main\0\0\0\0";
/// Packet type tag: File Description packet.
pub const TYPE_FILE_DESC: [u8; 16] = *b"PAR 2.0\0FileDesc";
/// Packet type tag: Input File Slice Checksum packet.
pub const TYPE_IFSC: [u8; 16] = *b"PAR 2.0\0IFSC\0\0\0\0";
/// Packet type tag: Recovery Slice packet.
pub const TYPE_RECOVERY: [u8; 16] = *b"PAR 2.0\0RecvSlic";
/// Packet type tag: Creator packet.
pub const TYPE_CREATOR: [u8; 16] = *b"PAR 2.0\0Creator\0";

/// MD5 + CRC32 checksum of one (zero-padded) input slice.
#[derive(Debug, Clone, Copy)]
pub struct SliceChecksum {
    pub md5: [u8; 16],
    pub crc32: u32,
}

/// Compute the MD5 digest of a byte slice.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(data);
    let mut out = [0u8; 16];
    out.copy_from_slice(&hasher.finalize());
    out
}

/// Zero-pad a byte vector up to the next multiple of 4.
fn pad_to_4(mut bytes: Vec<u8>) -> Vec<u8> {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
    bytes
}

/// Serialize a complete PAR2 packet: the 64-byte header followed by `body`.
///
/// # Panics
///
/// Panics if `body`'s length is not a multiple of 4.
pub fn serialize_packet(
    recovery_set_id: &[u8; 16],
    packet_type: &[u8; 16],
    body: &[u8],
) -> Vec<u8> {
    assert!(
        body.len().is_multiple_of(4),
        "PAR2 packet body must be a multiple of 4 bytes"
    );
    let total = HEADER_LEN + body.len();
    let mut packet = Vec::with_capacity(total);
    packet.extend_from_slice(&MAGIC); // 0..8
    packet.extend_from_slice(&(total as u64).to_le_bytes()); // 8..16
    packet.extend_from_slice(&[0u8; 16]); // 16..32 — MD5 placeholder
    packet.extend_from_slice(recovery_set_id); // 32..48
    packet.extend_from_slice(packet_type); // 48..64
    packet.extend_from_slice(body); // 64..

    let hash = md5(&packet[32..]);
    packet[16..32].copy_from_slice(&hash);
    packet
}

/// Build the body of a Main packet.
///
/// The recovery set contains exactly the given files (no non-recovery files).
/// File IDs are sorted so the body — and therefore the recovery set ID — is
/// canonical regardless of input order.
pub fn main_body(slice_size: u64, recovery_file_ids: &[[u8; 16]]) -> Vec<u8> {
    let mut sorted = recovery_file_ids.to_vec();
    sorted.sort_unstable();

    let mut body = Vec::with_capacity(12 + 16 * sorted.len());
    body.extend_from_slice(&slice_size.to_le_bytes());
    body.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    for id in &sorted {
        body.extend_from_slice(id);
    }
    body
}

/// The recovery set ID is the MD5 of the Main packet's body.
pub fn recovery_set_id(main_body: &[u8]) -> [u8; 16] {
    md5(main_body)
}

/// Compute a File ID: the MD5 of the 16k hash, the file length and the name.
pub fn compute_file_id(md5_16k: &[u8; 16], file_length: u64, name: &str) -> [u8; 16] {
    let mut input = Vec::with_capacity(24 + name.len());
    input.extend_from_slice(md5_16k);
    input.extend_from_slice(&file_length.to_le_bytes());
    input.extend_from_slice(name.as_bytes());
    md5(&input)
}

/// Build the body of a File Description packet.
pub fn file_description_body(
    file_id: &[u8; 16],
    md5_full: &[u8; 16],
    md5_16k: &[u8; 16],
    file_length: u64,
    name: &str,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(56 + name.len() + 3);
    body.extend_from_slice(file_id);
    body.extend_from_slice(md5_full);
    body.extend_from_slice(md5_16k);
    body.extend_from_slice(&file_length.to_le_bytes());
    body.extend_from_slice(name.as_bytes());
    pad_to_4(body)
}

/// Build the body of an Input File Slice Checksum packet.
pub fn ifsc_body(file_id: &[u8; 16], slices: &[SliceChecksum]) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + 20 * slices.len());
    body.extend_from_slice(file_id);
    for slice in slices {
        body.extend_from_slice(&slice.md5);
        body.extend_from_slice(&slice.crc32.to_le_bytes());
    }
    body
}

/// Build the body of a Recovery Slice packet.
///
/// # Panics
///
/// Panics if `data`'s length is not a multiple of 4.
pub fn recovery_body(exponent: u32, data: &[u8]) -> Vec<u8> {
    assert!(
        data.len().is_multiple_of(4),
        "recovery slice data must be a multiple of 4 bytes"
    );
    let mut body = Vec::with_capacity(4 + data.len());
    body.extend_from_slice(&exponent.to_le_bytes());
    body.extend_from_slice(data);
    body
}

/// Build the body of a Creator packet.
pub fn creator_body(creator: &str) -> Vec<u8> {
    pad_to_4(creator.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known_vectors() {
        assert_eq!(
            md5(b""),
            [
                0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
                0x42, 0x7e
            ]
        );
        assert_eq!(
            md5(b"abc"),
            [
                0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0, 0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1,
                0x7f, 0x72
            ]
        );
    }

    #[test]
    fn all_type_tags_are_16_bytes() {
        for tag in [
            TYPE_MAIN,
            TYPE_FILE_DESC,
            TYPE_IFSC,
            TYPE_RECOVERY,
            TYPE_CREATOR,
        ] {
            assert_eq!(tag.len(), 16);
            assert!(tag.starts_with(b"PAR 2.0\0"));
        }
    }

    #[test]
    fn serialized_packet_has_correct_header() {
        let rsid = [0x11u8; 16];
        let body = b"abcd1234"; // 8 bytes, multiple of 4
        let packet = serialize_packet(&rsid, &TYPE_CREATOR, body);

        assert_eq!(&packet[0..8], b"PAR2\0PKT");
        assert_eq!(
            u64::from_le_bytes(packet[8..16].try_into().unwrap()),
            (HEADER_LEN + body.len()) as u64
        );
        assert_eq!(&packet[32..48], &rsid);
        assert_eq!(&packet[48..64], &TYPE_CREATOR);
        assert_eq!(&packet[64..], body);
        // The stored hash covers everything from offset 32 onward.
        assert_eq!(&packet[16..32], &md5(&packet[32..]));
        assert!(packet.len().is_multiple_of(4));
    }

    #[test]
    fn main_body_sorts_file_ids_and_recovery_set_id_is_its_md5() {
        let ids = [[0x03u8; 16], [0x01u8; 16], [0x02u8; 16]];
        let body = main_body(768_000, &ids);

        assert_eq!(u64::from_le_bytes(body[0..8].try_into().unwrap()), 768_000);
        assert_eq!(u32::from_le_bytes(body[8..12].try_into().unwrap()), 3);
        // Sorted: 0x01.., 0x02.., 0x03..
        assert_eq!(&body[12..28], &[0x01u8; 16]);
        assert_eq!(&body[28..44], &[0x02u8; 16]);
        assert_eq!(&body[44..60], &[0x03u8; 16]);
        assert_eq!(recovery_set_id(&body), md5(&body));
    }

    #[test]
    fn file_id_is_md5_of_hash_length_and_name() {
        let md5_16k = [0xABu8; 16];
        let id = compute_file_id(&md5_16k, 1234, "movie.mkv");

        let mut expected_input = Vec::new();
        expected_input.extend_from_slice(&md5_16k);
        expected_input.extend_from_slice(&1234u64.to_le_bytes());
        expected_input.extend_from_slice(b"movie.mkv");
        assert_eq!(id, md5(&expected_input));
    }

    #[test]
    fn file_description_body_layout_and_padding() {
        let body = file_description_body(&[1; 16], &[2; 16], &[3; 16], 5000, "ab");
        assert_eq!(&body[0..16], &[1u8; 16]);
        assert_eq!(&body[16..32], &[2u8; 16]);
        assert_eq!(&body[32..48], &[3u8; 16]);
        assert_eq!(u64::from_le_bytes(body[48..56].try_into().unwrap()), 5000);
        assert_eq!(&body[56..58], b"ab");
        assert!(body.len().is_multiple_of(4)); // 58 -> padded to 60
        assert_eq!(body.len(), 60);
    }

    #[test]
    fn ifsc_body_packs_20_bytes_per_slice() {
        let slices = [
            SliceChecksum {
                md5: [9; 16],
                crc32: 0x1122_3344,
            },
            SliceChecksum {
                md5: [8; 16],
                crc32: 0xAABB_CCDD,
            },
        ];
        let body = ifsc_body(&[7; 16], &slices);
        assert_eq!(body.len(), 16 + 20 * 2);
        assert_eq!(&body[0..16], &[7u8; 16]);
        assert_eq!(&body[16..32], &[9u8; 16]);
        assert_eq!(
            u32::from_le_bytes(body[32..36].try_into().unwrap()),
            0x1122_3344
        );
    }

    #[test]
    fn recovery_body_prefixes_the_exponent() {
        let data = vec![0u8; 8];
        let body = recovery_body(42, &data);
        assert_eq!(u32::from_le_bytes(body[0..4].try_into().unwrap()), 42);
        assert_eq!(body.len(), 12);
    }

    #[test]
    fn creator_body_is_padded() {
        assert_eq!(creator_body("pesto"), b"pesto\0\0\0");
    }
}
