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

fn encode_s(data: &[u8], line_len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    encode_scalar(&mut out, data, line_len);
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
    assert_eq!(encode_s(&[0xD6], 128), vec![b'=', 0x40, b'\r', b'\n']);
    // 0xE3 + 42 == 0x0D (CR) -> escaped.
    assert_eq!(encode_s(&[0xE3], 128), vec![b'=', 0x4D, b'\r', b'\n']);
    // 0x13 + 42 == 0x3D ('=') -> escaped.
    assert_eq!(encode_s(&[0x13], 128), vec![b'=', 0x7D, b'\r', b'\n']);
}

#[test]
fn escapes_dot_at_line_start() {
    // 0x04 + 42 == 0x2E ('.'); at the start of a line it must be escaped.
    assert_eq!(encode_s(&[0x04], 128), vec![b'=', 0x6E, b'\r', b'\n']);
    // Mid-line, the same byte is left untouched.
    let line = encode_s(&[0x00, 0x04], 128);
    assert_eq!(line, vec![0x2A, 0x2E, b'\r', b'\n']);
}

#[test]
fn non_critical_byte_is_shifted() {
    // 0x00 + 42 == 0x2A ('*'), not a critical character.
    assert_eq!(encode_s(&[0x00], 128), vec![0x2A, b'\r', b'\n']);
}

#[test]
fn wraps_lines_at_line_length() {
    let encoded = encode_s(&[0u8; 5], 2);
    // Three lines: 2 + 2 + 1 bytes, each terminated by CRLF.
    assert_eq!(encoded, b"\x2a\x2a\r\n\x2a\x2a\r\n\x2a\r\n");
}

#[test]
fn encoding_is_reversible_for_all_byte_values() {
    let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
    assert_eq!(decode(&encode_s(&data, 128)), data);
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

#[test]
fn encode_part_empty_data_zero_length_segment() {
    // An empty file produces one segment with no data bytes.
    let part = encode_part(
        "empty.bin",
        0,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &[],
        128,
        None,
    );
    let body = String::from_utf8(part.body).unwrap();
    // Control lines still present and consistent.
    assert!(body.contains("size=0"));
    assert!(body.contains("=yend size=0 crc32="));
    // begin=1, end=0 for an empty part (offset + len = 0 + 0).
    assert_eq!(part.begin, 1);
    assert_eq!(part.end, 0);
    assert_eq!(part.crc32, crc32(&[]));
}

#[test]
fn segments_file_exactly_article_size_yields_one_segment() {
    assert_eq!(segments(128, 128), vec![(0, 128)]);
}

#[test]
fn segments_file_one_byte_over_article_size_yields_two_segments() {
    assert_eq!(segments(129, 128), vec![(0, 128), (128, 1)]);
}

#[test]
fn body_length_matches_line_length_exactly() {
    // A single segment whose data is exactly line_len bytes should produce
    // a single encoded line (no premature wrap, no missing trailing CRLF).
    let data: Vec<u8> = vec![0x00; 128]; // 128 bytes, none critical
    let encoded = encode_s(&data, 128);
    // Exactly 128 encoded bytes + CRLF; no mid-line CRLF.
    let newline_count = encoded.windows(2).filter(|w| w == b"\r\n").count();
    assert_eq!(
        newline_count, 1,
        "expected exactly one CRLF for a full line"
    );
    // Round-trip.
    assert_eq!(decode(&encoded), data);
}

// --- 26a: comprehensive coverage of all four critical bytes ---

// Input bytes whose encoded value (b+42 mod 256) hits each critical value.
const NUL_IN: u8 = 0xD6; // (0xD6+42)%256=0x00 NUL
const LF_IN: u8 = 0xE0; // (0xE0+42)%256=0x0A LF
const CR_IN: u8 = 0xE3; // (0xE3+42)%256=0x0D CR
const EQ_IN: u8 = 0x13; // (0x13+42)%256=0x3D '='
                        // Input bytes whose encoded value hits positional specials.
const TAB_IN: u8 = 0xDF; // (0xDF+42)%256=0x09 TAB
const SP_IN: u8 = 0xF6; // (0xF6+42)%256=0x20 SPACE
const DOT_IN: u8 = 0x04; // (0x04+42)%256=0x2E '.'

#[test]
fn all_critical_bytes_escaped_at_first_position() {
    for raw in [NUL_IN, LF_IN, CR_IN, EQ_IN] {
        let enc = encode_s(&[raw], 128);
        assert_eq!(enc[0], b'=', "byte {raw:#04x} not escaped at position 0");
    }
}

#[test]
fn all_critical_bytes_escaped_in_middle() {
    // Surround with a neutral byte (0x00 encodes to 0x2A, never critical).
    for raw in [NUL_IN, LF_IN, CR_IN, EQ_IN] {
        let data = [0x00, raw, 0x00];
        let enc = encode_s(&data, 128);
        // First byte is neutral (0x2A), second must be escape '='.
        assert_eq!(enc[0], 0x2A);
        assert_eq!(enc[1], b'=', "byte {raw:#04x} not escaped in middle");
    }
}

#[test]
fn all_critical_bytes_escaped_at_last_position() {
    for raw in [NUL_IN, LF_IN, CR_IN, EQ_IN] {
        let data = [0x00, raw];
        let enc = encode_s(&data, 128);
        assert_eq!(enc[0], 0x2A);
        assert_eq!(enc[1], b'=', "byte {raw:#04x} not escaped at last position");
    }
}

#[test]
fn consecutive_critical_bytes_all_escaped() {
    let data = [NUL_IN, LF_IN, CR_IN, EQ_IN];
    let enc = encode_s(&data, 128);
    // 4 escaped bytes = 8 encoded bytes + CRLF.
    assert_eq!(enc.len(), 10);
    for i in (0..8).step_by(2) {
        assert_eq!(enc[i], b'=', "escape marker missing at index {i}");
    }
    assert_eq!(&enc[8..], b"\r\n");
}

#[test]
fn critical_escape_payload_is_value_plus_64() {
    // NUL (0x00): escape payload = 0x00 + 64 = 0x40.
    assert_eq!(encode_s(&[NUL_IN], 128)[1], 0x40);
    // LF  (0x0A): payload = 0x4A.
    assert_eq!(encode_s(&[LF_IN], 128)[1], 0x4A);
    // CR  (0x0D): payload = 0x4D.
    assert_eq!(encode_s(&[CR_IN], 128)[1], 0x4D);
    // '=' (0x3D): payload = 0x7D.
    assert_eq!(encode_s(&[EQ_IN], 128)[1], 0x7D);
}

// --- 26a: positional escapes ---

#[test]
fn tab_escaped_at_line_start() {
    let enc = encode_s(&[TAB_IN], 128);
    assert_eq!(enc[0], b'=', "TAB not escaped at line start");
    assert_eq!(enc[1], 0x09u8.wrapping_add(64));
}

#[test]
fn tab_not_escaped_mid_line() {
    // 0x00 is a safe neutral byte (encodes to 0x2A).
    let data = [0x00, TAB_IN, 0x00];
    let enc = encode_s(&data, 128);
    assert_eq!(enc[1], 0x09, "TAB incorrectly escaped mid-line");
}

#[test]
fn tab_escapedat_line_end() {
    // line_len=4: positions 0,1,2,3 → position 3 is the last in the line.
    let data = [0x00, 0x00, 0x00, TAB_IN];
    let enc = encode_s(&data, 4);
    // First three neutral bytes produce 0x2A each.
    assert_eq!(&enc[..3], &[0x2A, 0x2A, 0x2A]);
    assert_eq!(enc[3], b'=', "TAB not escaped at line end (col=line_len-1)");
}

#[test]
fn space_escaped_at_line_start() {
    let enc = encode_s(&[SP_IN], 128);
    assert_eq!(enc[0], b'=', "SPACE not escaped at line start");
}

#[test]
fn space_not_escaped_mid_line() {
    let data = [0x00, SP_IN, 0x00];
    let enc = encode_s(&data, 128);
    assert_eq!(enc[1], 0x20, "SPACE incorrectly escaped mid-line");
}

#[test]
fn space_escapedat_line_end() {
    let data = [0x00, 0x00, 0x00, SP_IN];
    let enc = encode_s(&data, 4);
    assert_eq!(&enc[..3], &[0x2A, 0x2A, 0x2A]);
    assert_eq!(enc[3], b'=', "SPACE not escaped at line end");
}

#[test]
fn dot_escaped_at_start_of_second_line() {
    // line_len=2: first line = [0x00, 0x00], second line starts with DOT_IN.
    let data = [0x00, 0x00, DOT_IN];
    let enc = encode_s(&data, 2);
    // First line: 0x2A 0x2A \r\n
    assert_eq!(&enc[..4], b"\x2a\x2a\r\n");
    // Second line must begin with '=' (dot at line start, NNTP dot-stuffing).
    assert_eq!(enc[4], b'=', "dot not escaped at start of second line");
}

#[test]
fn dot_not_escaped_mid_line_or_line_end() {
    // DOT_IN mid-line and at end of line — neither position requires escape.
    let data = [0x00, DOT_IN, DOT_IN];
    let enc = encode_s(&data, 128);
    // Position 1 and 2 are mid-line and last — no escape needed.
    assert_eq!(enc[1], 0x2E, "dot incorrectly escaped mid-line");
    assert_eq!(enc[2], 0x2E, "dot incorrectly escaped at last position");
}

// --- 26a: wrap-around at exactly line_len ---

#[test]
fn line_wrap_inserts_crlf_at_boundary() {
    // line_len=3: every 3 input bytes produce one CRLF.
    let data = [0x00u8; 6];
    let enc = encode_s(&data, 3);
    assert_eq!(&enc[3..5], b"\r\n", "CRLF missing after first full line");
    assert_eq!(&enc[8..10], b"\r\n", "CRLF missing after second full line");
    assert_eq!(enc.len(), 10); // 3+CRLF + 3+CRLF
}

#[test]
fn trailing_crlf_added_for_partial_line() {
    let data = [0x00u8; 5];
    let enc = encode_s(&data, 3);
    // Lines: 3 bytes + CRLF, then 2 bytes + CRLF.
    assert_eq!(enc.len(), 9);
    assert_eq!(&enc[7..], b"\r\n");
}

// --- 26a: full 256-byte round-trip ---

#[test]
fn full_256_byte_round_trip() {
    let data: Vec<u8> = (0u8..=255).collect();
    assert_eq!(decode(&encode_s(&data, 128)), data);
}

// --- 26b: SSSE3 path produces identical output to scalar ---

#[cfg(target_arch = "x86_64")]
fn encode_ssse3_vec(data: &[u8], line_len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    encode_ssse3(&mut out, data, line_len);
    out
}

/// Macro: assert SSSE3 output equals scalar output for `data` and `line_len`.
#[allow(unused_macros)]
macro_rules! assert_ssse3_eq {
    ($data:expr, $line_len:expr) => {{
        #[cfg(target_arch = "x86_64")]
        {
            let scalar = encode_s($data, $line_len);
            let simd = encode_ssse3_vec($data, $line_len);
            assert_eq!(
                simd, scalar,
                "SSSE3 diverges from scalar (line_len={})",
                $line_len
            );
        }
    }};
}

#[test]
fn ssse3_matches_scalar_all_256_byte_values() {
    #[cfg(target_arch = "x86_64")]
    {
        let data: Vec<u8> = (0u8..=255).collect();
        assert_ssse3_eq!(&data, 128);
    }
}

#[test]
fn ssse3_matches_scalar_all_critical_bytes() {
    #[cfg(target_arch = "x86_64")]
    {
        // All four critical raw inputs, repeated to span many lines.
        let data: Vec<u8> = [NUL_IN, LF_IN, CR_IN, EQ_IN]
            .iter()
            .cycle()
            .copied()
            .take(512)
            .collect();
        assert_ssse3_eq!(&data, 128);
    }
}

#[test]
fn ssse3_matches_scalar_positional_bytes_at_boundaries() {
    #[cfg(target_arch = "x86_64")]
    {
        // Dot/space/tab at line start and end with line_len=4.
        let data: Vec<u8> = [DOT_IN, SP_IN, TAB_IN, 0x00]
            .iter()
            .cycle()
            .copied()
            .take(256)
            .collect();
        assert_ssse3_eq!(&data, 4);
    }
}

#[test]
fn ssse3_matches_scalar_large_random_like_payload() {
    #[cfg(target_arch = "x86_64")]
    {
        // 750 KB of pseudo-random data (covers the typical article size).
        let data: Vec<u8> = (0u8..=255)
            .cycle()
            .enumerate()
            .map(|(i, b): (usize, u8)| b.wrapping_add((i.wrapping_mul(7).wrapping_add(13)) as u8))
            .take(750 * 1024)
            .collect();
        assert_ssse3_eq!(&data, 128);
    }
}

#[test]
fn ssse3_matches_scalar_empty() {
    #[cfg(target_arch = "x86_64")]
    assert_ssse3_eq!(&[], 128);
}

#[test]
fn ssse3_matches_scalar_single_byte() {
    #[cfg(target_arch = "x86_64")]
    for b in 0u8..=255 {
        let data = [b];
        assert_ssse3_eq!(&data, 128);
    }
}

#[test]
fn ssse3_matches_scalar_short_line_len() {
    #[cfg(target_arch = "x86_64")]
    {
        // Stress the boundary logic with very short lines.
        let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        for ll in [1, 2, 3, 4, 7, 16, 17] {
            assert_ssse3_eq!(&data, ll);
        }
    }
}

#[test]
fn ssse3_round_trip() {
    #[cfg(target_arch = "x86_64")]
    {
        let data: Vec<u8> = (0u8..=255).cycle().take(750 * 1024).collect();
        let mut encoded = Vec::new();
        encode_ssse3(&mut encoded, &data, 128);
        assert_eq!(decode(&encoded), data);
    }
}

// --- 26c: AVX2 path produces identical output to scalar ---

#[cfg(target_arch = "x86_64")]
fn encode_avx2_vec(data: &[u8], line_len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    encode_avx2(&mut out, data, line_len);
    out
}

#[allow(unused_macros)]
macro_rules! assert_avx2_eq {
    ($data:expr, $line_len:expr) => {{
        #[cfg(target_arch = "x86_64")]
        {
            let scalar = encode_s($data, $line_len);
            let simd = encode_avx2_vec($data, $line_len);
            assert_eq!(
                simd, scalar,
                "AVX2 diverges from scalar (line_len={})",
                $line_len
            );
        }
    }};
}

#[test]
fn avx2_matches_scalar_all_256_byte_values() {
    #[cfg(target_arch = "x86_64")]
    {
        let data: Vec<u8> = (0u8..=255).collect();
        assert_avx2_eq!(&data, 128);
    }
}

#[test]
fn avx2_matches_scalar_all_critical_bytes() {
    #[cfg(target_arch = "x86_64")]
    {
        let data: Vec<u8> = [NUL_IN, LF_IN, CR_IN, EQ_IN]
            .iter()
            .cycle()
            .copied()
            .take(512)
            .collect();
        assert_avx2_eq!(&data, 128);
    }
}

#[test]
fn avx2_matches_scalar_positional_bytes_at_boundaries() {
    #[cfg(target_arch = "x86_64")]
    {
        let data: Vec<u8> = [DOT_IN, SP_IN, TAB_IN, 0x00]
            .iter()
            .cycle()
            .copied()
            .take(256)
            .collect();
        assert_avx2_eq!(&data, 4);
    }
}

#[test]
fn avx2_matches_scalar_large_random_like_payload() {
    #[cfg(target_arch = "x86_64")]
    {
        let data: Vec<u8> = (0u8..=255)
            .cycle()
            .enumerate()
            .map(|(i, b): (usize, u8)| b.wrapping_add((i.wrapping_mul(7).wrapping_add(13)) as u8))
            .take(750 * 1024)
            .collect();
        assert_avx2_eq!(&data, 128);
    }
}

#[test]
fn avx2_matches_scalar_empty() {
    #[cfg(target_arch = "x86_64")]
    assert_avx2_eq!(&[], 128);
}

#[test]
fn avx2_matches_scalar_single_byte() {
    #[cfg(target_arch = "x86_64")]
    for b in 0u8..=255 {
        let data = [b];
        assert_avx2_eq!(&data, 128);
    }
}

#[test]
fn avx2_matches_scalar_short_line_len() {
    #[cfg(target_arch = "x86_64")]
    {
        let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        for ll in [1, 2, 3, 4, 7, 16, 17, 32, 33] {
            assert_avx2_eq!(&data, ll);
        }
    }
}

#[test]
fn avx2_round_trip() {
    #[cfg(target_arch = "x86_64")]
    {
        let data: Vec<u8> = (0u8..=255).cycle().take(750 * 1024).collect();
        let mut encoded = Vec::new();
        encode_avx2(&mut encoded, &data, 128);
        assert_eq!(decode(&encoded), data);
    }
}

// --- dispatcher always matches scalar ---

#[test]
fn dispatcher_matches_scalar_large_payload() {
    let data: Vec<u8> = (0u8..=255).cycle().take(750 * 1024).collect();
    let mut a = Vec::new();
    let mut b = Vec::new();
    super::encode(&mut a, &data, 128);
    encode_scalar(&mut b, &data, 128);
    assert_eq!(a, b);
}

// --- 26d: encoded_size matches actual output length ---

fn check_encoded_size(data: &[u8], line_len: usize) {
    let predicted = encoded_size(data, line_len);
    let actual = encode_s(data, line_len).len();
    assert_eq!(
        predicted,
        actual,
        "encoded_size mismatch: predicted={predicted} actual={actual} \
         (data.len()={}, line_len={line_len})",
        data.len()
    );
}

#[test]
fn encoded_size_matches_empty() {
    assert_eq!(encoded_size(&[], 128), 0);
}

#[test]
fn encoded_size_matches_all_256_byte_values() {
    let data: Vec<u8> = (0u8..=255).collect();
    check_encoded_size(&data, 128);
}

#[test]
fn encoded_size_matches_all_critical_bytes() {
    let data: Vec<u8> = [NUL_IN, LF_IN, CR_IN, EQ_IN]
        .iter()
        .cycle()
        .copied()
        .take(512)
        .collect();
    check_encoded_size(&data, 128);
}

#[test]
fn encoded_size_matches_positional_bytes_at_boundaries() {
    let data: Vec<u8> = [DOT_IN, SP_IN, TAB_IN, 0x00]
        .iter()
        .cycle()
        .copied()
        .take(256)
        .collect();
    for ll in [1, 2, 3, 4, 7, 16, 17, 128] {
        check_encoded_size(&data, ll);
    }
}

#[test]
fn encoded_size_matches_all_single_bytes() {
    for b in 0u8..=255 {
        check_encoded_size(&[b], 128);
        check_encoded_size(&[b], 1);
    }
}

#[test]
fn encoded_size_matches_large_payload() {
    let data: Vec<u8> = (0u8..=255)
        .cycle()
        .enumerate()
        .map(|(i, b): (usize, u8)| b.wrapping_add((i.wrapping_mul(7).wrapping_add(13)) as u8))
        .take(750 * 1024)
        .collect();
    check_encoded_size(&data, 128);
}
