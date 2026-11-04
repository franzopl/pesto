//! Multipart geometry, allocation-bound, and decode-seam tests for the
//! encryption adapters.

use super::super::super::kdf::EncryptionSession;
use super::super::*;
use super::fixture_kdf_input;
use crate::yenc::PartSpec;
use std::sync::Arc;

// Body Standard v1.2 §2.1/§5: physical multipart geometry and pcrc32
// apply even when total=1. Build authenticated fixtures independently of
// the total-based ordinary decoder and leave that decoder unchanged.
fn explicit_multipart_fixture(
    session: &Arc<EncryptionSession>,
    file_size: u64,
    offset: u64,
) -> Vec<u8> {
    let encoded = UploadEncryptionAdapter::new(session.clone())
        .encode_article(
            "geometry.bin",
            file_size,
            PartSpec {
                number: 1,
                total: 2,
                offset,
            },
            b"test",
            128,
            1,
            &mut Vec::new(),
        )
        .unwrap();
    let restored = control::decrypt_yenc_control_lines(session, 1, &encoded.body).unwrap();
    replace_fixture_bytes(&restored, b"total=2", b"total=1")
}

fn replace_fixture_bytes(raw: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let pos = raw
        .windows(needle.len())
        .position(|bytes| bytes == needle)
        .unwrap();
    let mut out = raw.to_vec();
    out.splice(pos..pos + needle.len(), replacement.iter().copied());
    out
}

#[test]
fn total_one_physical_multipart_preserves_authenticated_geometry() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x42; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let raw = explicit_multipart_fixture(&session, 20, 4);
    let wire = control::encrypt_yenc_control_lines(&session, 1, &raw).unwrap();
    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(&wire)
        .unwrap();
    assert_eq!(decoded.data, b"test");
    assert_eq!(
        (decoded.begin, decoded.end, decoded.file_size, decoded.total),
        (5, 8, 20, 1)
    );
}

#[test]
fn total_one_physical_multipart_bad_geometry_releases_no_output() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x42; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let raw = explicit_multipart_fixture(&session, 4, 0);
    let adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    for range in [
        "begin=0 end=4",
        "begin=4 end=1",
        "begin=1 end=5",
        "begin=2 end=4",
        "begin=4294967296 end=4294967299",
        "begin=1 end=4294967296",
        "begin=oops end=4",
        "end=4",
        "begin=1",
    ] {
        let damaged = replace_fixture_bytes(&raw, b"begin=1 end=4", range.as_bytes());
        let wire = control::encrypt_yenc_control_lines(&session, 1, &damaged).unwrap();
        let err = adapter
            .decode_article(&wire)
            .expect_err("bad physical range must release no decoded output");
        assert!(
            err.to_string().contains("GEOMETRY")
                || err.to_string().contains("SIZE_OVERFLOW")
                || err.to_string().contains("=ypart")
                // T05: malformed range values now fail as INVALID_AAD_FIELD
                // at the AAD validation seam (Body Std v1.2 §2.1) — still a
                // no-output provider-corruption failure.
                || err.to_string().contains("INVALID_AAD_FIELD"),
            "{range}: {err}"
        );
    }
}

#[test]
fn total_one_physical_multipart_requires_pcrc32_despite_competing_crc32() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x42; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let raw = explicit_multipart_fixture(&session, 4, 0);
    let lines = control::split_lines_preserving_endings(&raw);
    let footer = std::str::from_utf8(lines.last().unwrap().content).unwrap();
    let pcrc = footer
        .split_whitespace()
        .find(|token| token.starts_with("pcrc32="))
        .unwrap();
    let crc = pcrc.strip_prefix("pcrc32=").unwrap();
    let adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    for bad_pcrc in ["", "pcrc32=invalid", "pcrc32=00000000"] {
        let damaged = replace_fixture_bytes(
            &raw,
            footer.as_bytes(),
            format!("=yend size=4 part=1 {bad_pcrc} crc32={crc}").as_bytes(),
        );
        let wire = control::encrypt_yenc_control_lines(&session, 1, &damaged).unwrap();
        let err = adapter
            .decode_article(&wire)
            .expect_err("crc32 cannot substitute for pcrc32");
        assert!(err.to_string().contains("CRC"), "{bad_pcrc}: {err}");
    }
    let competing = replace_fixture_bytes(
        &raw,
        footer.as_bytes(),
        format!("{footer} crc32=00000000").as_bytes(),
    );
    let wire = control::encrypt_yenc_control_lines(&session, 1, &competing).unwrap();
    assert_eq!(adapter.decode_article(&wire).unwrap().data, b"test");
}

#[test]
fn multipart_pcrc32_accepts_non_eight_hex_widths() {
    // Body Std v1.2 §5 step 5 / X-2: the wire pcrc32 is a hexadecimal
    // string of any width that parses as a CRC-32 value; it need not be
    // zero-padded to eight digits. Mint a multipart article whose
    // ciphertext CRC has a leading zero nibble (so a genuinely short
    // form exists), then rewrite the restored footer pcrc32= to a
    // 10-digit leading-zero form and to the short minimal form, and
    // require `decode_article` to succeed with the plaintext both times.
    // This needs the crate-private restore/re-encrypt codec, so it
    // lives here rather than in penne's public-seam integration tests.
    let (kdf_input, salt) = (fixture_kdf_input(), [0x42; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let adapter = DownloadDecryptionAdapter::with_password(kdf_input);

    // Deterministic search for a payload whose ciphertext pcrc32 has a
    // zero top nibble (probability 1/16 per candidate).
    let mut fixture = None;
    for i in 0..512 {
        let payload = format!("pcrc width payload {i}").into_bytes();
        let encoded = UploadEncryptionAdapter::new(session.clone())
            .encode_article(
                "width.bin",
                payload.len() as u64 * 2,
                PartSpec {
                    number: 1,
                    total: 2,
                    offset: 0,
                },
                &payload,
                128,
                1,
                &mut Vec::new(),
            )
            .unwrap();
        let restored = control::decrypt_yenc_control_lines(&session, 1, &encoded.body).unwrap();
        let lines = control::split_lines_preserving_endings(&restored);
        let footer = std::str::from_utf8(lines.last().unwrap().content).unwrap();
        let pcrc = footer
            .split_whitespace()
            .find_map(|token| token.strip_prefix("pcrc32="))
            .unwrap();
        let value = u32::from_str_radix(pcrc, 16).unwrap();
        if value < 0x1000_0000 {
            fixture = Some((payload, restored, value));
            break;
        }
    }
    let (payload, restored, pcrc) =
        fixture.expect("a leading-zero pcrc32 candidate must occur within 512 payloads");
    let canonical = format!("pcrc32={pcrc:08x}");
    assert!(restored
        .windows(canonical.len())
        .any(|w| w == canonical.as_bytes()));

    for formatted in [format!("pcrc32={pcrc:010x}"), format!("pcrc32={pcrc:x}")] {
        let width = formatted.len() - "pcrc32=".len();
        assert!(
            width == 10 || width < 8,
            "rewritten pcrc32 must be non-canonical width, got {width} digits: {formatted}"
        );
        let damaged = replace_fixture_bytes(&restored, canonical.as_bytes(), formatted.as_bytes());
        let wire = control::encrypt_yenc_control_lines(&session, 1, &damaged).unwrap();
        let decoded = adapter
            .decode_article(&wire)
            .unwrap_or_else(|e| panic!("pcrc32 width {width} ({formatted}) must decode: {e}"));
        assert_eq!(decoded.data, payload, "pcrc32 width {width} plaintext");
    }
}

/// An encrypted article with single-part total=1
/// and an explicit =ypart line (e.g. from nyuu single-part posts) is tolerated
/// on the encrypted decode path: =ypart is skipped before yenc::decode_part.
#[test]
fn tolerates_ypart_in_single_part_article() {
    let kdf_input = fixture_kdf_input();
    let payload = b"nyuu-style encrypted single-part framing payload";
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());

    let segment_index = 1u32;
    let nonce = session.derive_body_nonce(segment_index);
    let aad = body::format_body_aad(1, 1, payload.len() as u64, b"single.bin");
    let (ciphertext, tag) =
        body::encrypt_body(payload, &aad, session.master_key(), &nonce).unwrap();

    let encoded = yenc::encode_part(
        "single.bin",
        payload.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &ciphertext,
        128,
        None,
    );

    // Rebuild body with injected =ypart (line 2) and =yencryption (line 3)
    let lines = control::split_lines_preserving_endings(&encoded.body);
    let mut raw_body = Vec::new();
    // Line 1: =ybegin with nyuu-style part=1 total=1
    raw_body.extend_from_slice(
        format!(
            "=ybegin part=1 total=1 line=128 size={} name=single.bin\r\n",
            payload.len()
        )
        .as_bytes(),
    );
    // Line 2: injected =ypart
    raw_body.extend_from_slice(format!("=ypart begin=1 end={}\r\n", payload.len()).as_bytes());
    // Data lines
    for line in &lines[1..lines.len() - 1] {
        raw_body.extend_from_slice(line.content);
        raw_body.extend_from_slice(b"\r\n");
    }
    // Physical multipart framing requires pcrc32, even with total=1.
    raw_body.extend_from_slice(
        format!(
            "=yend size={} part=1 pcrc32={:08x}\r\n",
            ciphertext.len(),
            yenc::crc32(&ciphertext)
        )
        .as_bytes(),
    );

    insert_yencryption_line(&mut raw_body, &salt, &tag, segment_index, true).unwrap();
    // FF1-encrypt control lines
    let wire_body =
        control::encrypt_yenc_control_lines(&session, segment_index, &raw_body).unwrap();

    let download_adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    let decoded = download_adapter
        .decode_article(&wire_body)
        .expect("single-part with =ypart must decode on encrypted path");
    assert_eq!(decoded.data, payload);
    assert_eq!(decoded.total, 1);
    assert_eq!(decoded.part, 1);
}

#[test]
fn huge_physical_multipart_range_fails_without_panic_or_output() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x42; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let raw = explicit_multipart_fixture(&session, 20, 4);
    let adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    for total in ["total=1", "total=2"] {
        let raw = replace_fixture_bytes(&raw, b"total=1", total.as_bytes());
        let damaged =
            replace_fixture_bytes(&raw, b"begin=5 end=8", b"begin=1 end=18446744073709551615");
        let wire = control::encrypt_yenc_control_lines(&session, 1, &damaged).unwrap();
        let result = std::panic::catch_unwind(|| adapter.decode_article(&wire))
            .expect("untrusted range must never panic");
        let error = result.expect_err("untrusted range must release no output");
        assert!(error.to_string().contains("PROVIDER_FAILOVER"), "{error}");
    }
}

#[test]
fn encrypted_multipart_yend_never_carries_whole_file_crc() {
    // Body Std v1.2 §5 step 5: a multipart =yend carries only pcrc32, even on
    // the last part when the caller supplies a whole-file CRC.
    let session = Arc::new(
        EncryptionSession::new(fixture_kdf_input(), control::generate_alphabet_salt()).unwrap(),
    );
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());
    let payload = b"last part of a two part file";
    let mut body = Vec::new();
    let encoded = upload_adapter
        .encode_article(
            "two.bin",
            100,
            PartSpec {
                number: 2,
                total: 2,
                offset: 72,
            },
            payload,
            128,
            2,
            &mut body,
        )
        .unwrap();

    let clear = control::decrypt_yenc_control_lines(&session, 2, &encoded.body).unwrap();
    let text = String::from_utf8_lossy(&clear);
    let yend = text
        .lines()
        .find(|line| line.starts_with("=yend"))
        .expect("restored article has a =yend line");
    assert!(yend.contains(" pcrc32="), "{yend}");
    assert!(!yend.contains(" crc32="), "{yend}");
}

// The article-sized `size=` override in `clean_for_decode` replaces the
// existing header keyword span instead of appending a duplicate token, so the
// internal decode seam never relies on last-wins keyword tie-breaking.
#[test]
fn filename_containing_size_text_round_trips_with_single_size_token() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x71u8; 16]);
    let payload = b"name token safety";
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let encoded = UploadEncryptionAdapter::new(session.clone())
        .encode_article(
            "my size=999 report.bin",
            payload.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128,
            1,
            &mut Vec::new(),
        )
        .unwrap();

    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(&encoded.body)
        .expect("filename containing size= text must not disturb the seam");
    assert_eq!(decoded.name, "my size=999 report.bin");
    assert_eq!(decoded.data, payload);
    assert_eq!(decoded.file_size, payload.len() as u64);
    assert_eq!(decoded.begin, 1);
    assert_eq!(decoded.end, payload.len() as u64);

    // The synthetic header handed to the ordinary decoder carries exactly
    // one size= token; the filename text (after name=) is untouched.
    let restored = control::decrypt_yenc_control_lines(&session, 1, &encoded.body).unwrap();
    let (_, clean) = extract_and_remove_yencryption(&restored).unwrap();
    let first = control::split_lines_preserving_endings(&clean)[0]
        .content
        .to_vec();
    let name_idx = first.windows(5).position(|w| w == b"name=").unwrap();
    let header = std::str::from_utf8(&first[..name_idx]).unwrap();
    assert_eq!(
        header
            .split_whitespace()
            .filter(|t| t.starts_with("size="))
            .count(),
        1,
        "synthetic =ybegin must carry a single size= token: {header}"
    );
}

#[test]
fn seam_replacement_preserves_geometry_for_multipart_and_total_one() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x72u8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    // Physical multipart framing (an =ypart line) with total=1: the
    // article-sized override must still drive bounded decoding while the
    // authenticated whole-file geometry is restored afterwards.
    let payload = b"test";
    let file_size = 400_000_000u64;
    let encoded = UploadEncryptionAdapter::new(session.clone())
        .encode_article(
            "geometry size=99.bin",
            file_size,
            PartSpec {
                number: 1,
                total: 2,
                offset: 0,
            },
            payload,
            128,
            1,
            &mut Vec::new(),
        )
        .unwrap();
    let restored = control::decrypt_yenc_control_lines(&session, 1, &encoded.body).unwrap();
    let wire = control::encrypt_yenc_control_lines(&session, 1, &restored).unwrap();

    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(&wire)
        .expect("physical multipart with total=2 must decode");
    assert_eq!(decoded.data, payload);
    assert_eq!(decoded.file_size, file_size);
    assert_eq!(decoded.begin, 1);
    assert_eq!(decoded.end, 4);
    assert_eq!(decoded.total, 2);

    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(
            &control::encrypt_yenc_control_lines(
                &session,
                1,
                &replace_fixture_bytes(&restored, b"total=2", b"total=1"),
            )
            .unwrap(),
        )
        .expect("physical multipart with total=1 must decode identically");
    assert_eq!(decoded.data, payload);
    assert_eq!(decoded.file_size, file_size);
    assert_eq!(decoded.begin, 1);
    assert_eq!(decoded.end, 4);
}

#[test]
fn large_file_tiny_segment_allocates_bounded() {
    // Legal large-file/tiny-segment geometry: the synthetic article-sized
    // size= (replaced in place) keeps the ordinary decoder's allocation at
    // the segment length, not the declared whole-file size.
    let (kdf_input, salt) = (fixture_kdf_input(), [0x73u8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"tiny";
    let file_size = 4_000_000_000u64; // legal: under the 4 GiB (2^32) guard
    let encoded = UploadEncryptionAdapter::new(session.clone())
        .encode_article(
            "big.bin",
            file_size,
            PartSpec {
                number: 1,
                total: 2,
                offset: 0,
            },
            payload,
            128,
            1,
            &mut Vec::new(),
        )
        .unwrap();
    let restored = control::decrypt_yenc_control_lines(&session, 1, &encoded.body).unwrap();
    let wire = control::encrypt_yenc_control_lines(&session, 1, &restored).unwrap();

    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(&wire)
        .expect("tiny segment of a large legal file must decode");
    assert_eq!(decoded.data, payload);
    assert_eq!(decoded.file_size, file_size);
    assert_eq!(decoded.begin, 1);
    assert_eq!(decoded.end, 4);
}
