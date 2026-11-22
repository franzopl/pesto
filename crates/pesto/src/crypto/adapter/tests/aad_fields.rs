//! T05 regression: required numeric AAD input validation at the encrypted
//! decode seam (Body Encryption Standard v1.2 §2.1).
//!
//! The ordinary yEnc parser silently selects duplicate keywords (last-wins)
//! and silently defaults missing `part=`/`total=` to 1. For an encrypted
//! article those values feed the AAD, so an ambiguous or missing value must
//! fail before ordinary parsing can discard the ambiguity. These tests use
//! the known-key mutation technique: the article is encoded with the upload
//! adapter, control lines are restored with the session key, the restored
//! `=ybegin`/`=ypart` text is mutated, and the control lines are re-encrypted
//! with the body tag left unchanged.

use super::super::super::kdf::EncryptionSession;
use super::super::*;
use super::fixture_kdf_input;
use crate::yenc::PartSpec;
use std::sync::Arc;

/// Encode an encrypted article, restore its control lines with the session
/// key, and return the plaintext-framing bytes ready for mutation.
fn restored_article(
    session: &Arc<EncryptionSession>,
    name: &str,
    file_size: u64,
    spec: PartSpec,
    payload: &[u8],
) -> Vec<u8> {
    let encoded = UploadEncryptionAdapter::new(session.clone())
        .encode_article(name, file_size, spec, payload, 128, 1, &mut Vec::new())
        .unwrap();
    control::decrypt_yenc_control_lines(session, 1, &encoded.body).unwrap()
}

/// Replace the first occurrence of `old` with `new` inside the line whose
/// content starts with `line_prefix`, preserving line endings.
fn mutate_line(raw: &[u8], line_prefix: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + new.len());
    let mut replaced = false;
    for line in control::split_lines_preserving_endings(raw) {
        if !replaced && line.content.starts_with(line_prefix) {
            let pos = line
                .content
                .windows(old.len())
                .position(|w| w == old)
                .unwrap_or_else(|| {
                    panic!("mutation target {old:?} not found on {line_prefix:?} line")
                });
            out.extend_from_slice(&line.content[..pos]);
            out.extend_from_slice(new);
            out.extend_from_slice(&line.content[pos + old.len()..]);
            replaced = true;
        } else {
            out.extend_from_slice(line.content);
        }
        out.extend_from_slice(line.ending);
    }
    assert!(replaced, "no {line_prefix:?} line in fixture");
    out
}

/// Run a mutation table: mutate the restored fixture, re-encrypt control
/// lines, and require `decode_article` to fail (or pass for controls)
/// without ever releasing the plaintext.
fn assert_mutations(
    session: &Arc<EncryptionSession>,
    raw: &[u8],
    must_pass: &[(&[u8], &[u8], &[u8])],
    must_fail: &[(&[u8], &[u8], &[u8])],
    payload: &[u8],
) {
    let adapter = DownloadDecryptionAdapter::with_password(fixture_kdf_input());
    for (line_prefix, old, new) in must_pass {
        let damaged = mutate_line(raw, line_prefix, old, new);
        let wire = control::encrypt_yenc_control_lines(session, 1, &damaged).unwrap();
        let decoded = adapter
            .decode_article(&wire)
            .unwrap_or_else(|e| panic!("same-value duplicate control {new:?} must pass: {e}"));
        assert_eq!(decoded.data, payload);
    }
    for (line_prefix, old, new) in must_fail {
        let damaged = mutate_line(raw, line_prefix, old, new);
        let wire = control::encrypt_yenc_control_lines(session, 1, &damaged).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            adapter.decode_article(&wire)
        }))
        .expect("malformed numeric AAD input must never panic");
        let err = result.expect_err("ambiguous/missing numeric AAD input must release no output");
        let msg = err.to_string();
        // Zero-Output Guarantee (Body Std v1.2 §7): the failure must not
        // leak the plaintext payload.
        assert!(
            !msg.as_bytes().windows(payload.len()).any(|w| w == payload),
            "error leaked plaintext for mutation {new:?}: {msg}"
        );
    }
}

#[test]
fn ybegin_size_ambiguity_fails_and_same_value_duplicates_pass() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x7du8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"data";
    let raw = restored_article(
        &session,
        "f.bin",
        4,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload,
    );
    assert_mutations(
        &session,
        &raw,
        &[
            // Controls: same-value duplicates stay legal.
            (b"=ybegin", b"size=4", b"size=4 size=4"),
        ],
        &[
            // Conflicting duplicates in both orders.
            (b"=ybegin", b"size=4", b"size=5 size=4"),
            (b"=ybegin", b"size=4", b"size=4 size=5"),
            // Malformed occurrence beside a valid duplicate.
            (b"=ybegin", b"size=4", b"size=oops size=4"),
            (b"=ybegin", b"size=4", b"size=4 size=oops"),
            // Overflowing values, alone and beside a valid duplicate.
            (b"=ybegin", b"size=4", b"size=18446744073709551616"),
            (b"=ybegin", b"size=4", b"size=18446744073709551616 size=4"),
            (b"=ybegin", b"size=4", b"size=4294967296"),
            // Missing required size.
            (b"=ybegin", b"size=4", b""),
            // Empty value.
            (b"=ybegin", b"size=4", b"size="),
        ],
        payload,
    );
}

#[test]
fn multipart_part_ambiguity_fails_and_same_value_duplicates_pass() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x7eu8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"data";
    let raw = restored_article(
        &session,
        "f.bin",
        8,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        payload,
    );
    assert_mutations(
        &session,
        &raw,
        &[(b"=ybegin", b"part=1", b"part=1 part=1")],
        &[
            // Conflicting part= duplicates in both orders.
            (b"=ybegin", b"part=1", b"part=2 part=1"),
            (b"=ybegin", b"part=1", b"part=1 part=2"),
            // Malformed part (ordinary parser would silently default to 1).
            (b"=ybegin", b"part=1", b"part=oops"),
            (b"=ybegin", b"part=1", b"part=oops part=1"),
            // Missing required multipart part=.
            (b"=ybegin", b"part=1", b""),
            // Overflowing u32 part, alone and beside a valid duplicate.
            (b"=ybegin", b"part=1", b"part=4294967296"),
            (b"=ybegin", b"part=1", b"part=4294967296 part=1"),
        ],
        payload,
    );
}

#[test]
fn ypart_range_ambiguity_fails_and_same_value_duplicates_pass() {
    let (kdf_input, salt) = (fixture_kdf_input(), [0x7fu8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"data";
    let raw = restored_article(
        &session,
        "f.bin",
        8,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        payload,
    );
    assert_mutations(
        &session,
        &raw,
        &[
            (b"=ypart", b"begin=1", b"begin=1 begin=1"),
            (b"=ypart", b"end=4", b"end=4 end=4"),
        ],
        &[
            // Conflicting begin/end duplicates in both orders.
            (b"=ypart", b"begin=1", b"begin=2 begin=1"),
            (b"=ypart", b"begin=1", b"begin=1 begin=2"),
            (b"=ypart", b"end=4", b"end=4 end=5"),
            (b"=ypart", b"end=4", b"end=5 end=4"),
            // Malformed/overflowing/missing range values.
            (b"=ypart", b"begin=1", b"begin=oops"),
            (b"=ypart", b"begin=1", b"begin=18446744073709551616"),
            (b"=ypart", b"begin=1", b""),
            (b"=ypart", b"end=4", b"end=4294967296"),
            (b"=ypart", b"end=4", b""),
        ],
        payload,
    );
}

#[test]
fn duplicate_yencryption_in_data_region_of_restored_article_is_rejected() {
    // GAP13-01 (reworked, test-only): a second `=yencryption` line appearing
    // after the first data line of the *restored* article is in the data
    // region, not the header — `extract_and_remove_yencryption` must fail
    // closed with "duplicate or misplaced =yencryption" and return no output
    // (Body Std v1.2 §5/§7). The duplicate is the article's real
    // `=yencryption` line, injected as a plaintext line immediately after
    // the first data line of the restored fixture. This pins the restored-
    // side rejection unit directly; at wire level `decode_article` still
    // fails closed for such articles (generically — see below), it just
    // must not surface this specific restored-side message there.
    let (kdf_input, salt) = (fixture_kdf_input(), [0x7cu8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    // Long enough (after yEnc expansion of the ciphertext) to span at
    // least two data lines at line length 128.
    let payload: &[u8] = &[b'x'; 200];
    let raw = restored_article(
        &session,
        "f.bin",
        payload.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload,
    );
    // Single-part restored layout: =ybegin, =yencryption, data..., =yend.
    let lines = control::split_lines_preserving_endings(&raw);
    assert!(lines[0].content.starts_with(b"=ybegin"));
    assert!(lines[1].content.starts_with(b"=yencryption"));
    assert!(
        lines.len() >= 5,
        "fixture must have header, at least two data lines and a footer"
    );
    let duplicate = lines[1].content.to_vec();
    // Insert the duplicate after the first data line (line index 2).
    let first_data = 2;
    let mut restored = Vec::with_capacity(raw.len() + duplicate.len() + 2);
    for (idx, line) in lines.iter().enumerate() {
        restored.extend_from_slice(line.content);
        restored.extend_from_slice(line.ending);
        if idx == first_data {
            restored.extend_from_slice(&duplicate);
            restored.extend_from_slice(line.ending);
        }
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        extract_and_remove_yencryption(&restored)
    }))
    .expect("duplicate =yencryption in data region must never panic");
    let err = result.expect_err("duplicate =yencryption in data region must release no output");
    let msg = err.to_string();
    assert!(
        msg.contains("duplicate or misplaced =yencryption"),
        "expected duplicate/misplaced =yencryption error, got: {msg}"
    );
    assert!(
        !msg.as_bytes().windows(payload.len()).any(|w| w == payload),
        "error leaked plaintext: {msg}"
    );
    // Optional wire-level companion (generic fail-closed only): feeding the
    // same tampered article through `decode_article` (after control-line
    // re-encryption) must still release no output, with any error — not
    // necessarily the restored-side duplicate message.
    let wire = control::encrypt_yenc_control_lines(&session, 1, &restored).unwrap();
    let wire_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        DownloadDecryptionAdapter::with_password(kdf_input).decode_article(&wire)
    }))
    .expect("wire-level duplicate must never panic");
    let wire_err =
        wire_result.expect_err("wire-level duplicate =yencryption must release no output");
    let wire_msg = wire_err.to_string();
    assert!(
        !wire_msg
            .as_bytes()
            .windows(payload.len())
            .any(|w| w == payload),
        "wire-level error leaked plaintext: {wire_msg}"
    );
}

#[test]
fn filename_token_text_is_never_interpreted_as_a_field() {
    // Controls: filenames embedding `size=` / `part=` token text must pass
    // even when the embedded text looks like the malformed mutations above;
    // the validation only reads the header region before `name=`.
    let (kdf_input, salt) = (fixture_kdf_input(), [0x5du8; 16]);
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"data";
    for name in [
        "my size=999 report.bin",
        "ep part=oops.bin",
        "size=5 size=4.bin",
        "part=2 part=1.mkv",
    ] {
        let raw = restored_article(
            &session,
            name,
            4,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
        );
        let wire = control::encrypt_yenc_control_lines(&session, 1, &raw).unwrap();
        let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
            .decode_article(&wire)
            .unwrap_or_else(|e| panic!("filename {name:?} must not trip field validation: {e}"));
        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.name, name);
    }

    // A one-part multipart article (physical framing, part=1/total=2) and a
    // single-part article remain valid without mutation.
    for spec in [
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
    ] {
        let raw = restored_article(&session, "ok.bin", 4, spec, payload);
        let wire = control::encrypt_yenc_control_lines(&session, 1, &raw).unwrap();
        let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
            .decode_article(&wire)
            .expect("unmutated article must decode");
        assert_eq!(decoded.data, payload);
    }
}
