//! yEnc encoder.
//!
//! Encodes raw file bytes into yEnc article bodies, including the `=ybegin`,
//! `=ypart` and `=yend` control lines, and splits files into multi-part
//! segments. This is the hot path — the inner [`encode_into`] loop allocates
//! nothing beyond the output buffer.
//!
//! Reference: the yEnc draft specification, <http://www.yenc.org/yenc-draft.1.3.txt>.

/// Default yEnc line length, in source bytes per line.
pub const DEFAULT_LINE_LENGTH: usize = 128;

// --- CRC-32 (IEEE polynomial, reflected — the variant used by yEnc) ---

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC32_TABLE: [u32; 256] = build_crc32_table();

/// Incremental CRC-32 calculator (IEEE polynomial, as used by yEnc).
#[derive(Debug, Clone)]
pub struct Crc32 {
    state: u32,
}

impl Crc32 {
    /// Start a fresh CRC-32 computation.
    pub fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    /// Feed more bytes into the running checksum.
    pub fn update(&mut self, data: &[u8]) {
        let mut state = self.state;
        for &b in data {
            state = CRC32_TABLE[((state ^ b as u32) & 0xFF) as usize] ^ (state >> 8);
        }
        self.state = state;
    }

    /// Produce the final CRC-32 value.
    pub fn finalize(&self) -> u32 {
        !self.state
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// CRC-32 of a complete byte slice.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(data);
    crc.finalize()
}

// --- Segmentation ---

/// Split a file of `file_size` bytes into `(offset, len)` segments of at most
/// `article_size` bytes each. An empty file yields a single empty segment so
/// it still produces one article.
pub fn segments(file_size: u64, article_size: usize) -> Vec<(u64, usize)> {
    let article_size = article_size.max(1) as u64;
    if file_size == 0 {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut offset = 0u64;
    while offset < file_size {
        let len = article_size.min(file_size - offset);
        out.push((offset, len as usize));
        offset += len;
    }
    out
}

// --- Encoding ---

/// Identifies one segment of a file to be encoded.
#[derive(Debug, Clone, Copy)]
pub struct PartSpec {
    /// 1-based part number.
    pub number: u32,
    /// Total number of parts for the file.
    pub total: u32,
    /// 0-based byte offset of this part within the file.
    pub offset: u64,
}

/// A yEnc-encoded segment, ready to be used as an NNTP article body.
#[derive(Debug)]
pub struct EncodedPart {
    /// 1-based part number.
    pub number: u32,
    /// Total number of parts for the file.
    pub total: u32,
    /// 1-based offset of the first byte of this part within the file.
    pub begin: u64,
    /// 1-based offset of the last byte of this part within the file.
    pub end: u64,
    /// CRC-32 of this part's raw bytes.
    pub crc32: u32,
    /// Full yEnc body: `=ybegin` / `=ypart` / data lines / `=yend`.
    pub body: Vec<u8>,
}

/// Encode one segment of a file into a complete yEnc article body.
///
/// `data` is the raw byte slice for this part. `file_size` is the size of the
/// whole file. When `spec.total > 1` a multi-part body is produced (with a
/// `=ypart` line); otherwise a single-part body is produced. `file_crc32`, if
/// supplied, is appended to the `=yend` line of multi-part articles.
pub fn encode_part(
    name: &str,
    file_size: u64,
    spec: PartSpec,
    data: &[u8],
    line_len: usize,
    file_crc32: Option<u32>,
) -> EncodedPart {
    let multipart = spec.total > 1;
    let part_crc = crc32(data);
    let begin = spec.offset + 1;
    let end = spec.offset + data.len() as u64;

    // Encoded data is roughly the same size as the input, plus escapes and
    // line endings; reserve generously to avoid reallocations on the hot path.
    let mut body = Vec::with_capacity(data.len() + data.len() / 32 + 128);

    if multipart {
        body.extend_from_slice(
            format!(
                "=ybegin part={} total={} line={} size={} name={}\r\n",
                spec.number, spec.total, line_len, file_size, name
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("=ypart begin={} end={}\r\n", begin, end).as_bytes());
    } else {
        body.extend_from_slice(
            format!(
                "=ybegin line={} size={} name={}\r\n",
                line_len, file_size, name
            )
            .as_bytes(),
        );
    }

    encode_into(&mut body, data, line_len);

    if multipart {
        body.extend_from_slice(
            format!(
                "=yend size={} part={} pcrc32={:08x}",
                data.len(),
                spec.number,
                part_crc
            )
            .as_bytes(),
        );
        if let Some(file_crc) = file_crc32 {
            body.extend_from_slice(format!(" crc32={:08x}", file_crc).as_bytes());
        }
        body.extend_from_slice(b"\r\n");
    } else {
        body.extend_from_slice(
            format!("=yend size={} crc32={:08x}\r\n", data.len(), part_crc).as_bytes(),
        );
    }

    EncodedPart {
        number: spec.number,
        total: spec.total,
        begin,
        end,
        crc32: part_crc,
        body,
    }
}

/// Core yEnc encoder: append the encoded form of `data` to `out`, wrapping
/// every `line_len` source bytes.
///
/// Each byte is shifted by 42 (mod 256). The four critical output values —
/// NUL, LF, CR and `=` — are always escaped as `=` followed by the value
/// shifted by a further 64. TAB and space are escaped only at the start or end
/// of a line (where transports may strip them), and `.` is escaped at the
/// start of a line (to keep clear of NNTP dot-stuffing).
fn encode_into(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    let line_len = line_len.max(1);
    let last = data.len().saturating_sub(1);
    let mut col = 0usize;

    for (i, &b) in data.iter().enumerate() {
        let at_line_start = col == 0;
        let at_line_end = col + 1 == line_len || i == last;
        let e = b.wrapping_add(42);

        let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
        let positional = ((e == 0x09 || e == 0x20) && (at_line_start || at_line_end))
            || (e == 0x2E && at_line_start);

        if critical || positional {
            out.push(b'=');
            out.push(e.wrapping_add(64));
        } else {
            out.push(e);
        }

        col += 1;
        if col == line_len {
            out.extend_from_slice(b"\r\n");
            col = 0;
        }
    }

    if col != 0 {
        out.extend_from_slice(b"\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal yEnc decoder used to verify that encoding is reversible.
    fn decode(encoded: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut escape = false;
        for &c in encoded {
            if escape {
                out.push(c.wrapping_sub(64).wrapping_sub(42));
                escape = false;
            } else if c == b'=' {
                escape = true;
            } else if c != b'\r' && c != b'\n' {
                out.push(c.wrapping_sub(42));
            }
        }
        out
    }

    fn encode(data: &[u8], line_len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        encode_into(&mut out, data, line_len);
        out
    }

    #[test]
    fn crc32_standard_check_value() {
        // The well-known CRC-32 check value for the ASCII string "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn crc32_incremental_matches_oneshot() {
        let data: Vec<u8> = (0..=255u8).collect();
        let mut crc = Crc32::new();
        crc.update(&data[..100]);
        crc.update(&data[100..]);
        assert_eq!(crc.finalize(), crc32(&data));
    }

    #[test]
    fn escapes_critical_characters() {
        // 0xD6 + 42 == 0x00 (NUL) -> escaped as '=' + (0x00 + 64).
        assert_eq!(encode(&[0xD6], 128), vec![b'=', 0x40, b'\r', b'\n']);
        // 0xE3 + 42 == 0x0D (CR) -> escaped.
        assert_eq!(encode(&[0xE3], 128), vec![b'=', 0x4D, b'\r', b'\n']);
        // 0x13 + 42 == 0x3D ('=') -> escaped.
        assert_eq!(encode(&[0x13], 128), vec![b'=', 0x7D, b'\r', b'\n']);
    }

    #[test]
    fn escapes_dot_at_line_start() {
        // 0x04 + 42 == 0x2E ('.'); at the start of a line it must be escaped.
        assert_eq!(encode(&[0x04], 128), vec![b'=', 0x6E, b'\r', b'\n']);
        // Mid-line, the same byte is left untouched.
        let line = encode(&[0x00, 0x04], 128);
        assert_eq!(line, vec![0x2A, 0x2E, b'\r', b'\n']);
    }

    #[test]
    fn non_critical_byte_is_shifted() {
        // 0x00 + 42 == 0x2A ('*'), not a critical character.
        assert_eq!(encode(&[0x00], 128), vec![0x2A, b'\r', b'\n']);
    }

    #[test]
    fn wraps_lines_at_line_length() {
        let encoded = encode(&[0u8; 5], 2);
        // Three lines: 2 + 2 + 1 bytes, each terminated by CRLF.
        assert_eq!(encoded, b"\x2a\x2a\r\n\x2a\x2a\r\n\x2a\r\n");
    }

    #[test]
    fn encoding_is_reversible_for_all_byte_values() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        assert_eq!(decode(&encode(&data, 128)), data);
    }

    #[test]
    fn segments_split_evenly_and_with_remainder() {
        assert_eq!(segments(250, 100), vec![(0, 100), (100, 100), (200, 50)]);
        assert_eq!(segments(100, 100), vec![(0, 100)]);
        assert_eq!(segments(0, 100), vec![(0, 0)]);
    }

    #[test]
    fn single_part_body_has_expected_control_lines() {
        let part = encode_part(
            "test.bin",
            3,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            &[1, 2, 3],
            128,
            None,
        );
        let body = String::from_utf8(part.body).unwrap();
        assert!(body.starts_with("=ybegin line=128 size=3 name=test.bin\r\n"));
        assert!(body.contains("=yend size=3 crc32="));
        assert_eq!(part.begin, 1);
        assert_eq!(part.end, 3);
    }

    #[test]
    fn multi_part_body_has_ypart_line() {
        let part = encode_part(
            "movie.mkv",
            10_000,
            PartSpec {
                number: 2,
                total: 3,
                offset: 100,
            },
            &[0u8; 50],
            128,
            Some(0xDEAD_BEEF),
        );
        let body = String::from_utf8(part.body).unwrap();
        assert!(body.starts_with("=ybegin part=2 total=3 line=128 size=10000 name=movie.mkv\r\n"));
        assert!(body.contains("=ypart begin=101 end=150\r\n"));
        assert!(body.contains("=yend size=50 part=2 pcrc32="));
        assert!(body.contains("crc32=deadbeef"));
        assert_eq!(part.begin, 101);
        assert_eq!(part.end, 150);
    }
}
