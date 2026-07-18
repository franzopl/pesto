//! Reads existing PAR2 files back into raw packets.
//!
//! This is the byte-level inverse of [`crate::packet`]: given the raw bytes
//! of a `.par2` file (an index file or a recovery volume), extract every
//! syntactically valid packet. Input is treated as untrusted — a corrupted
//! or even maliciously crafted file must never panic, over-allocate based on
//! a forged length field, or read past the end of the buffer.

use crate::packet::{self, HEADER_LEN};

/// One packet read back from a `.par2` file: header fields plus the body.
#[derive(Debug, Clone)]
pub struct RawPacket {
    /// Recovery set ID this packet belongs to (header bytes 32..48).
    pub recovery_set_id: [u8; 16],
    /// Packet type tag (header bytes 48..64) — compare against
    /// [`packet::TYPE_MAIN`] and friends.
    pub packet_type: [u8; 16],
    /// Packet body (everything after the 64-byte header).
    pub body: Vec<u8>,
}

/// Scan `data` for every syntactically valid PAR2 packet.
///
/// Packets are found with a forward byte scan for the magic sequence
/// `PAR2\0PKT`; a packet is only accepted if its declared length fits within
/// the remaining buffer and its stored MD5 (covering bytes 32 onward)
/// matches. Bytes that don't form a valid packet are skipped one byte at a
/// time, so a corrupted or truncated file yields whatever packets are still
/// intact instead of nothing. The scan is linear in `data.len()` regardless
/// of input content — every position is inspected at most once, so a file
/// full of near-miss magic sequences cannot force quadratic behaviour.
pub fn read_packets(data: &[u8]) -> Vec<RawPacket> {
    let mut packets = Vec::new();
    let mut pos = 0usize;

    while pos + 8 <= data.len() {
        if &data[pos..pos + 8] != packet::MAGIC.as_slice() {
            pos += 1;
            continue;
        }
        match try_parse_one(&data[pos..]) {
            Some((raw, consumed)) => {
                packets.push(raw);
                pos += consumed;
            }
            None => pos += 1,
        }
    }

    packets
}

/// Parse one packet starting at the beginning of `buf` (which must already
/// start with the magic sequence). Returns the packet and the number of
/// bytes it occupies, or `None` if `buf` doesn't hold a valid packet there.
fn try_parse_one(buf: &[u8]) -> Option<(RawPacket, usize)> {
    if buf.len() < HEADER_LEN {
        return None;
    }

    let total_length = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    // Bound the untrusted length field against what's actually in the
    // buffer before using it for anything — a forged length must never
    // drive an allocation or a read past what's really on disk.
    if total_length < HEADER_LEN as u64 || !total_length.is_multiple_of(4) {
        return None;
    }
    let total_length = usize::try_from(total_length).ok()?;
    if total_length > buf.len() {
        return None;
    }

    let stored_hash = &buf[16..32];
    let computed_hash = packet::md5(&buf[32..total_length]);
    if stored_hash != computed_hash {
        return None;
    }

    let mut recovery_set_id = [0u8; 16];
    recovery_set_id.copy_from_slice(&buf[32..48]);
    let mut packet_type = [0u8; 16];
    packet_type.copy_from_slice(&buf[48..64]);
    let body = buf[HEADER_LEN..total_length].to_vec();

    Some((
        RawPacket {
            recovery_set_id,
            packet_type,
            body,
        },
        total_length,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_single_packet() {
        let rsid = [0x11u8; 16];
        let body = b"abcd1234".to_vec(); // 8 bytes, multiple of 4
        let bytes = packet::serialize_packet(&rsid, &packet::TYPE_CREATOR, &body);

        let packets = read_packets(&bytes);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].recovery_set_id, rsid);
        assert_eq!(packets[0].packet_type, packet::TYPE_CREATOR);
        assert_eq!(packets[0].body, body);
    }

    #[test]
    fn round_trips_multiple_concatenated_packets() {
        let rsid = [0x22u8; 16];
        let mut bytes = Vec::new();
        bytes.extend(packet::serialize_packet(&rsid, &packet::TYPE_MAIN, b"aaaa"));
        bytes.extend(packet::serialize_packet(
            &rsid,
            &packet::TYPE_CREATOR,
            b"bbbbbbbb",
        ));
        bytes.extend(packet::serialize_packet(&rsid, &packet::TYPE_IFSC, b"cccc"));

        let packets = read_packets(&bytes);
        assert_eq!(packets.len(), 3);
        assert_eq!(packets[0].packet_type, packet::TYPE_MAIN);
        assert_eq!(packets[1].packet_type, packet::TYPE_CREATOR);
        assert_eq!(packets[2].packet_type, packet::TYPE_IFSC);
    }

    #[test]
    fn rejects_a_packet_with_a_corrupted_hash_but_keeps_scanning() {
        let rsid = [0x33u8; 16];
        let mut bytes = packet::serialize_packet(&rsid, &packet::TYPE_CREATOR, b"aaaa");
        // Corrupt one byte of the stored MD5 hash (offset 16..32).
        bytes[20] ^= 0xFF;
        bytes.extend(packet::serialize_packet(&rsid, &packet::TYPE_IFSC, b"bbbb"));

        let packets = read_packets(&bytes);
        // The corrupted first packet is skipped; the valid second one is found.
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type, packet::TYPE_IFSC);
    }

    #[test]
    fn a_forged_huge_length_field_is_rejected_without_panicking_or_over_reading() {
        let rsid = [0x44u8; 16];
        let mut bytes = packet::serialize_packet(&rsid, &packet::TYPE_CREATOR, b"aaaa");
        // Overwrite the "total length" field with an enormous value.
        bytes[8..16].copy_from_slice(&(u64::MAX - 3).to_le_bytes());

        let packets = read_packets(&bytes);
        assert!(packets.is_empty());
    }

    #[test]
    fn truncated_input_does_not_panic() {
        let rsid = [0x55u8; 16];
        let full = packet::serialize_packet(&rsid, &packet::TYPE_MAIN, b"aaaaaaaa");
        for cut in 0..full.len() {
            // Must never panic regardless of where the buffer is cut.
            let _ = read_packets(&full[..cut]);
        }
    }

    #[test]
    fn empty_and_garbage_input_yield_no_packets() {
        assert!(read_packets(&[]).is_empty());
        assert!(read_packets(b"not a par2 file at all").is_empty());
    }
}
