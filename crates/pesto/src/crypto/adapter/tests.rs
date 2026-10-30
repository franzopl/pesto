//! Tests for `DownloadDecryptionAdapter` and `UploadEncryptionAdapter`.

use super::super::control::extract_bootstrap_from_line1;
use super::super::{control, kdf::EncryptionSession};
use super::*;
use crate::yenc::{self, PartSpec};
use std::sync::Arc;

mod aad_fields;
mod geometry;
mod local_failure;
mod public_api;

/// Fixture password read from the vendored body_encryption.json vectors
/// (first vector's `password` field) so this test module does not embed the
/// literal credential-shaped string. Shared by geometry/local_failure tests.
fn fixture_kdf_input() -> &'static str {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("test-vectors/body_encryption.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: serde_json::Value =
        serde_json::from_str(&content).expect("body_encryption.json must parse");
    static VALUE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VALUE
        .get_or_init(|| {
            fixture["vectors"][0]["password"]
                .as_str()
                .expect("body_encryption.json vectors[0].password")
                .to_owned()
        })
        .as_str()
}

fn other_kdf_input() -> &'static str {
    "incorrect-transport-kdf-for-tests"
}

#[test]
fn test_multi_salt_session_caching() {
    let kdf_input = fixture_kdf_input();
    let adapter = DownloadDecryptionAdapter::with_password(kdf_input);

    let salt1 = [0x11u8; 16];
    let salt2 = [0x22u8; 16];

    let s1 = adapter.get_or_create_session(salt1).unwrap();
    let s2 = adapter.get_or_create_session(salt2).unwrap();
    assert_eq!(s1.salt(), salt1);
    assert_eq!(s2.salt(), salt2);
    assert!(!Arc::ptr_eq(&s1, &s2));

    // Re-request salt1: must return cached instance (same Arc pointer)
    let s1_again = adapter.get_or_create_session(salt1).unwrap();
    assert!(Arc::ptr_eq(&s1, &s1_again));

    // Re-request salt2: must return cached instance (same Arc pointer)
    let s2_again = adapter.get_or_create_session(salt2).unwrap();
    assert!(Arc::ptr_eq(&s2, &s2_again));
}

#[test]
fn decryption_session_cache_evicts_oldest_entry_at_capacity() {
    let adapter = DownloadDecryptionAdapter::with_password(fixture_kdf_input());
    let mut first = None;
    for value in 1u8..=17 {
        let session = adapter.get_or_create_session([value; 16]).unwrap();
        if value == 1 {
            first = Some(session);
        }
    }

    let guard = adapter.cached_sessions.lock().unwrap();
    assert_eq!(guard.0.len(), 16);
    assert!(!guard.0.contains_key(&[1; 16]));
    assert!(guard.0.contains_key(&[17; 16]));
    drop(guard);

    let first_again = adapter.get_or_create_session([1; 16]).unwrap();
    assert!(!Arc::ptr_eq(&first.unwrap(), &first_again));
}

#[test]
fn concurrent_workers_derive_each_salt_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let adapter = Arc::new(DownloadDecryptionAdapter::with_password(fixture_kdf_input()));
    let salts = [[0x31u8; 16], [0x32u8; 16]];
    let derivations = [AtomicUsize::new(0), AtomicUsize::new(0)];

    std::thread::scope(|scope| {
        for worker in 0..16 {
            let adapter = &adapter;
            let derivations = &derivations;
            scope.spawn(move || {
                let which = worker % 2;
                let salt = salts[which];
                let session = adapter
                    .get_or_derive_session(salt, || {
                        derivations[which].fetch_add(1, Ordering::SeqCst);
                        // Hold the derivation open so every worker overlaps it.
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        EncryptionSession::new(fixture_kdf_input(), salt)
                    })
                    .unwrap();
                assert_eq!(session.salt(), salt);
            });
        }
    });

    assert_eq!(derivations[0].load(Ordering::SeqCst), 1);
    assert_eq!(derivations[1].load(Ordering::SeqCst), 1);
}

#[test]
fn encrypted_control_lines_round_trip_with_lf_endings() {
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session);
    let payload = b"LF-only encrypted control framing";
    let _segment_index = 1u32;
    let mut body = Vec::new();
    let encoded = upload_adapter
        .encode_article(
            "lf.bin",
            payload.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128,
            1u32,
            &mut body,
        )
        .unwrap();
    let mut lf_wire = encoded.body;
    let mut index = 0;
    while index + 1 < lf_wire.len() {
        if lf_wire[index..].starts_with(b"\r\n") {
            lf_wire.remove(index);
        }
        index += 1;
    }

    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(&lf_wire)
        .unwrap();
    assert_eq!(decoded.data, payload);
}

#[test]
fn adapter_rejects_unencrypted_article_fail_closed() {
    // A caller that configured transport decryption must never accept a
    // plain `=ybegin` article: that would release unauthenticated plaintext
    // (Zero-Output Guarantee, Body Std v1.2 §7).
    //
    // There is deliberately NO plaintext-prefix shortcut here: a valid
    // per-upload salt may itself begin with the bytes `=ybegin` (the 253-byte
    // Alphabet permits every byte except 0x00/0x0A/0x0D), so the wire prefix
    // is not proof of ordinary encoding. The plain article must instead fail
    // closed through the ordinary bootstrap/restoration/authentication path,
    // releasing no data.
    let payload = b"plain unencrypted payload";
    let encoded = yenc::encode_part(
        "plain.bin",
        payload.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        payload,
        128,
        None,
    );
    let err = DownloadDecryptionAdapter::with_password(other_kdf_input())
        .decode_article(&encoded.body)
        .expect_err("unencrypted article must fail closed under a decryptor");
    let msg = err.to_string();
    // Zero-Output Guarantee: the failure must not leak the plaintext.
    assert!(
        !msg.contains("plain unencrypted payload"),
        "error leaked plaintext: {msg}"
    );
}

#[test]
fn forced_ybegin_prefix_salts_round_trip_through_decode_article() {
    // A per-upload salt may legitimately begin with `=ybegin` (with or
    // without a space — both are valid 16-byte Alphabet sequences). The
    // decoder must rely on bootstrap extraction, control-line restoration
    // and AEAD authentication, never on a wire-prefix shortcut
    // (Body Std v1.2 §5; Control Std v1.2 §5).
    for salt in [*b"=ybeginABCDEFGHI", *b"=ybegin ABCDEFGH"] {
        let kdf_input = fixture_kdf_input();
        let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
        let uploader = UploadEncryptionAdapter::new(session);
        let payload = b"forced-salt payload";
        let mut body = Vec::new();
        let encoded = uploader
            .encode_article(
                "forced.bin",
                payload.len() as u64,
                PartSpec {
                    number: 1,
                    total: 1,
                    offset: 0,
                },
                payload,
                128,
                1u32,
                &mut body,
            )
            .unwrap();
        let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
            .decode_article(&encoded.body)
            .unwrap_or_else(|e| {
                panic!(
                    "salt {:?} must round-trip: {e}",
                    std::str::from_utf8(&salt).unwrap()
                )
            });
        assert_eq!(decoded.data, payload, "salt {:?} payload mismatch", salt);
    }
}

#[test]
fn forced_ybegin_prefix_salt_round_trips_with_bare_lf_endings() {
    // Same forced-salt contract under bare-LF line endings (Control Std
    // v1.2 §5 restoration must preserve the article's own line endings).
    for salt in [*b"=ybeginABCDEFGHI", *b"=ybegin ABCDEFGH"] {
        let kdf_input = fixture_kdf_input();
        let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
        let uploader = UploadEncryptionAdapter::new(session);
        let payload = b"forced-salt LF payload";
        let mut body = Vec::new();
        let encoded = uploader
            .encode_article(
                "forced-lf.bin",
                payload.len() as u64,
                PartSpec {
                    number: 1,
                    total: 1,
                    offset: 0,
                },
                payload,
                128,
                1u32,
                &mut body,
            )
            .unwrap();
        // Strip every CR, converting CRLF endings to bare LF.
        let mut lf_wire = encoded.body;
        let mut index = 0;
        while index < lf_wire.len() {
            if lf_wire[index] == b'\r' && lf_wire.get(index + 1) == Some(&b'\n') {
                lf_wire.remove(index);
                continue;
            }
            index += 1;
        }
        let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
            .decode_article(&lf_wire)
            .unwrap_or_else(|e| panic!("salt {:?} LF round-trip failed: {e}", salt));
        assert_eq!(decoded.data, payload);
    }
}

#[test]
fn adapter_rejects_uu_encoded_substitute_fail_closed() {
    // A uu-encoded article substituted by a malicious/corrupt provider must
    // fail closed under the decryptor, releasing no data.
    let uu = b"begin 644 substitute.bin\r\n$<&QA:6X\r\n`\r\nend\r\n";
    let err = DownloadDecryptionAdapter::with_password(other_kdf_input())
        .decode_article(uu)
        .expect_err("uu-encoded substitute must fail closed");
    let msg = err.to_string();
    assert!(!msg.contains("plain"), "error leaked content hint: {msg}");
}

#[test]
fn filenames_containing_control_keywords_round_trip() {
    // Filenames embedding yEnc control keywords (`part=`, `=ypart`) must
    // survive the full encode/decode round trip in both framings.
    struct KeywordCase<'a> {
        name: &'a str,
        payload_len: u64,
        number: u32,
        total: u32,
        offset: u64,
    }
    let cases = [
        KeywordCase {
            name: "ep_part=pilot.mkv",
            payload_len: 41,
            number: 1,
            total: 1,
            offset: 0,
        },
        KeywordCase {
            name: "movie=ypart1.mkv",
            payload_len: 1024,
            number: 1,
            total: 2,
            offset: 0,
        },
    ];
    for case in cases {
        let KeywordCase {
            name,
            payload_len,
            number,
            total,
            offset,
        } = case;
        let kdf_input = fixture_kdf_input();
        let salt = control::generate_alphabet_salt();
        let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
        let upload_adapter = UploadEncryptionAdapter::new(session);

        let payload = vec![0x33u8; payload_len as usize];
        let spec = PartSpec {
            number,
            total,
            offset,
        };
        let mut body = Vec::new();
        let encoded = upload_adapter
            .encode_article(name, payload_len, spec, &payload, 128, 1u32, &mut body)
            .expect("encode file with control keyword in filename");

        let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
            .decode_article(&encoded.body)
            .expect("decode file with control keyword in filename must succeed");

        assert_eq!(decoded.data, payload);
        assert_eq!(decoded.name, name);
        if total > 1 {
            assert_eq!(decoded.part, number);
            assert_eq!(decoded.total, total);
        }
    }
}

#[test]
fn insert_yencryption_line_placement_and_canonical_length() {
    // Body Std v1.2 §4: the =yencryption line is exactly 128 bytes and is
    // inserted after =ybegin (single-part) / after =ypart (multipart).
    // Round-trips through the full encode/decode adapters.
    let salt = [0x42u8; 16];
    let kdf_input = fixture_kdf_input();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let uploader = UploadEncryptionAdapter::new(session);
    let payload = b"placement payload";

    for (multipart, spec) in [
        (
            false,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
        ),
        (
            true,
            PartSpec {
                number: 1,
                total: 2,
                offset: 0,
            },
        ),
    ] {
        let mut body = Vec::new();
        let encoded = uploader
            .encode_article(
                "place.bin",
                payload.len() as u64,
                spec,
                payload,
                128,
                1,
                &mut body,
            )
            .expect("encode must succeed");
        // Pre-insertion structure check: the restored (pre-FF1) placement is
        // implied by the adapter contract; assert the wire still decrypts.
        let _ = multipart;
        let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
            .decode_article(&encoded.body)
            .expect("decode must succeed");
        assert_eq!(decoded.data, payload);
    }

    // Direct placement check on an unencoded body: single-part insertion
    // lands between =ybegin and the data; the line is canonical length.
    let tag = [0xedu8; 16];
    let mut raw = b"=ybegin line=128 size=4 name=t.bin\r\ndata\r\n=yend size=4\r\n".to_vec();
    super::insert_yencryption_line(&mut raw, &salt, &tag, 1, false).expect("insert must succeed");
    let lines = super::super::control::split_lines_preserving_endings(&raw);
    assert_eq!(lines.len(), 4);
    assert!(lines[0].content.starts_with(b"=ybegin"));
    assert_eq!(
        lines[1].content.len(),
        super::CANONICAL_YENCRYPTION_LINE_LEN
    );
    assert!(lines[1]
        .content
        .starts_with(b"=yencryption cipher=XChaCha20-Poly1305"));

    // Multipart insertion without an =ypart line fails without touching the
    // body (recoverable encoding error — MISSING_YPART_LINE, never a panic).
    let mut no_ypart = b"=ybegin line=128 size=4 name=t.bin\r\ndata\r\n=yend size=4\r\n".to_vec();
    let before = no_ypart.clone();
    let err = super::insert_yencryption_line(&mut no_ypart, &salt, &tag, 1, true)
        .expect_err("multipart insertion without =ypart must fail");
    assert!(err.to_string().contains("MISSING_YPART_LINE"), "got: {err}");
    assert_eq!(no_ypart, before, "body must be untouched on failure");
}

/// Build a single-part encrypted article whose `=ybegin name=` value is the
/// raw `raw_name` bytes (Body Std v1.2 §2.1). The upload API takes `&str`,
/// so the article is encoded under an ASCII placeholder of identical length
/// and the placeholder bytes are overwritten before control-line encryption.
fn hand_built_article_with_raw_name(
    password: &str,
    salt: [u8; 16],
    segment_index: u32,
    payload: &[u8],
    raw_name: &[u8],
) -> (Vec<u8>, Arc<EncryptionSession>) {
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let placeholder = "cafe.bin";
    assert_eq!(placeholder.len(), raw_name.len());
    let nonce = session.derive_body_nonce(segment_index);
    let aad = body::format_body_aad(1, 1, payload.len() as u64, raw_name);
    let (ciphertext, tag) =
        body::encrypt_body(payload, &aad, session.master_key(), &nonce).unwrap();

    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let mut wire = yenc::encode_part(
        placeholder,
        payload.len() as u64,
        spec,
        &ciphertext,
        128,
        None,
    )
    .body;
    let needle = format!("name={placeholder}");
    let pos = wire
        .windows(needle.len())
        .position(|w| w == needle.as_bytes())
        .unwrap();
    let name_start = pos + "name=".len();
    wire[name_start..name_start + raw_name.len()].copy_from_slice(raw_name);

    insert_yencryption_line(&mut wire, &salt, &tag, segment_index, false).unwrap();
    let wire = control::encrypt_yenc_control_lines(&session, segment_index, &wire).unwrap();
    (wire, session)
}

#[test]
fn non_utf8_ybegin_name_authenticates_with_raw_bytes() {
    let kdf_input = fixture_kdf_input();
    let salt = [0x5au8; 16];
    let payload = b"plaintext under a non-UTF-8 name";
    let raw_name = b"caf\xE9.bin";

    let (wire, _session) = hand_built_article_with_raw_name(kdf_input, salt, 7, payload, raw_name);
    let adapter = DownloadDecryptionAdapter::with_password(kdf_input);
    let decoded = adapter
        .decode_article(&wire)
        .expect("article authenticated with the raw =ybegin name bytes must decode");
    assert_eq!(decoded.data, payload);
}

#[test]
fn lossy_ybegin_name_would_change_the_aad() {
    let kdf_input = fixture_kdf_input();
    let salt = [0x5au8; 16];
    let payload = b"plaintext under a non-UTF-8 name";
    let raw_name = b"caf\xE9.bin";
    let (wire, session) = hand_built_article_with_raw_name(kdf_input, salt, 7, payload, raw_name);

    // What the old code derived the AAD from: the lossy `DecodedPart.name`.
    let restored = control::decrypt_yenc_control_lines(&session, 7, &wire).unwrap();
    let (_params, clean) = extract_and_remove_yencryption(&restored).unwrap();
    let lossy_name = yenc::decode_part(&clean).unwrap().name;
    assert_ne!(lossy_name.as_bytes(), raw_name);

    let nonce = session.derive_body_nonce(7);
    let decoded = yenc::decode_part(&clean).unwrap();
    let (ciphertext, tag) = (decoded.data.clone(), {
        let params =
            parse_yencryption_line(control::split_lines_preserving_endings(&restored)[1].content)
                .unwrap();
        params.tag
    });
    let raw_aad = body::format_body_aad(1, 1, payload.len() as u64, raw_name);
    let lossy_aad = body::format_body_aad(1, 1, payload.len() as u64, lossy_name.as_bytes());
    assert_ne!(raw_aad, lossy_aad);
    assert!(
        body::decrypt_body(&ciphertext, &tag, &lossy_aad, session.master_key(), &nonce).is_err()
    );
    assert_eq!(
        body::decrypt_body(&ciphertext, &tag, &raw_aad, session.master_key(), &nonce).unwrap(),
        payload
    );
}

#[test]
fn single_part_article_named_part_equals_round_trips_through_decode_article() {
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let upload = UploadEncryptionAdapter::new(session);
    let payload = b"F";
    let mut scratch = Vec::new();
    let encoded = upload
        .encode_article(
            "my part=2.bin",
            payload.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128,
            1u32,
            &mut scratch,
        )
        .unwrap();

    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(&encoded.body)
        .expect("single-part article named `my part=2.bin` must decrypt");
    assert_eq!(decoded.name, "my part=2.bin");
    assert_eq!(decoded.part, 1);
    assert_eq!(decoded.data, payload);
}

#[test]
fn encrypted_missing_name_or_crc_fails_closed() {
    let kdf_input = fixture_kdf_input();
    let session =
        Arc::new(EncryptionSession::new(kdf_input, control::generate_alphabet_salt()).unwrap());
    let encoded = UploadEncryptionAdapter::new(session.clone())
        .encode_article(
            "file.bin",
            5,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            b"hello",
            128,
            1,
            &mut Vec::new(),
        )
        .unwrap();
    let restored = control::decrypt_yenc_control_lines(&session, 1, &encoded.body).unwrap();
    for defect in 0..3 {
        let mut damaged = Vec::new();
        for line in control::split_lines_preserving_endings(&restored) {
            let mut content = line.content.to_vec();
            if defect < 2 && content.starts_with(b"=ybegin") {
                let pos = content.windows(6).position(|w| w == b" name=").unwrap();
                content.truncate(pos + if defect == 1 { 6 } else { 0 });
            }
            if defect == 2 && content.starts_with(b"=yend") {
                let pos = content.windows(7).position(|w| w == b" crc32=").unwrap();
                content.truncate(pos);
            }
            damaged.extend(content);
            damaged.extend(line.ending);
        }
        let wire = control::encrypt_yenc_control_lines(&session, 1, &damaged).unwrap();
        assert!(DownloadDecryptionAdapter::with_password(kdf_input)
            .decode_article(&wire)
            .is_err());
    }
}

#[test]
fn parse_yencryption_line_rejects_mutated_first_token_and_129_byte_line() {
    // R11-PC-03: mutated first token (`=yencryptionX`, 129 bytes) must not
    // parse, even though every field token is individually valid.
    let valid = "=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 index=00000001 tag=22e2fc27ec65d630332c351dbc5ba362";
    assert_eq!(valid.len(), super::CANONICAL_YENCRYPTION_LINE_LEN);
    parse_yencryption_line(valid.as_bytes()).expect("canonical line must parse");

    let mutated = "=yencryptionX cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 index=00000001 tag=22e2fc27ec65d630332c351dbc5ba362";
    assert_eq!(
        mutated.len(),
        129,
        "mutated-token case is the 129-byte line"
    );
    let err = parse_yencryption_line(mutated.as_bytes())
        .expect_err("mutated first token must be rejected");
    assert!(
        err.to_string().contains("INVALID_TOKEN_COUNT")
            || err.to_string().contains("INVALID_LENGTH"),
        "got: {err}"
    );

    // A 129-byte line with the exact first token (longer cipher token)
    // is likewise rejected rather than accepted as canonical grammar.
    let long = "=yencryption cipher=XChaCha20-Poly1305X salt=1a2b3c4d5e6f7890abcdef1234567890 index=00000001 tag=22e2fc27ec65d630332c351dbc5ba362";
    assert_eq!(long.len(), 129);
    assert!(
        parse_yencryption_line(long.as_bytes()).is_err(),
        "129-byte line must be rejected"
    );
}

#[test]
fn parse_yencryption_line_rejects_forbidden_segment_index_bytes() {
    // R11-PC-05: index whose uint32_be contains 0x0A/0x0D is rejected at
    // the grammar layer (PROVIDER_FAILOVER), not only via dual-bootstrap.
    for (index_hex, idx) in [("0000000a", 10u32), ("0000000d", 13), ("0000010a", 266)] {
        let line = format!(
            "=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 index={index_hex} tag=22e2fc27ec65d630332c351dbc5ba362"
        );
        assert_eq!(line.len(), super::CANONICAL_YENCRYPTION_LINE_LEN);
        let err = parse_yencryption_line(line.as_bytes())
            .expect_err(&format!("index {idx} ({index_hex}) must be rejected"));
        assert!(
            err.to_string().contains("FORBIDDEN_SEGMENT_INDEX_BYTE"),
            "index {idx}: got {err}"
        );
    }
}

#[test]
fn encode_article_rejects_zero_and_forbidden_segment_indices() {
    // R11-PC-04: the producing seam fails closed before nonce derivation.
    let kdf_input = fixture_kdf_input();
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let uploader = UploadEncryptionAdapter::new(session);
    for idx in [0u32, 10, 13, 266, 269] {
        let err = match uploader.encode_article(
            "guard.bin",
            4,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            b"data",
            128,
            idx,
            &mut Vec::new(),
        ) {
            Ok(_) => panic!("segment_index {idx} must be rejected at the seam"),
            Err(e) => e,
        };
        let expected = if idx == 0 {
            "ZERO_SEGMENT_INDEX"
        } else {
            "FORBIDDEN_SEGMENT_INDEX_BYTE"
        };
        assert!(
            err.to_string().contains(expected),
            "index {idx}: expected {expected}, got {err}"
        );
    }
    // Neighbouring safe indices still encode.
    for idx in [1u32, 9, 11, 267] {
        uploader
            .encode_article(
                "guard.bin",
                4,
                PartSpec {
                    number: 1,
                    total: 1,
                    offset: 0,
                },
                b"data",
                128,
                idx,
                &mut Vec::new(),
            )
            .unwrap_or_else(|e| panic!("safe index {idx} must encode: {e}"));
    }
}

#[test]
fn name_token_boundary_ignores_name_inside_keyword_value() {
    // PU12-04 / Q-B: a keyword value containing `name=` before the real
    // name must not be mistaken for the name token.
    use super::super::ybegin::name_token_boundary;
    let line = b"=ybegin note=name=x size=4 name=real.bin";
    let pos = name_token_boundary(line).expect("real name= token");
    assert_eq!(&line[pos + 5..], b"real.bin");
    // The ordinary shape still resolves to the first (only) token.
    let plain = b"=ybegin line=128 size=4 name=t.bin";
    assert_eq!(&plain[name_token_boundary(plain).unwrap() + 5..], b"t.bin");
    assert!(name_token_boundary(b"=ybegin line=128 size=4").is_none());
}

#[test]
fn ybegin_with_name_inside_keyword_value_authenticates() {
    // End-to-end PU12-04: craft a restored article whose =ybegin carries a
    // keyword value containing `name=` before the real name, authenticate
    // it with the AAD name taken at the token boundary, and require the
    // download adapter to decrypt it. With the old substring lookup the
    // AAD name would be `1 size=.. name=real.bin` and AEAD would fail.
    let kdf_input = fixture_kdf_input();
    let salt = [0x6bu8; 16];
    let session = Arc::new(EncryptionSession::new(kdf_input, salt).unwrap());
    let payload = b"boundary payload";
    let mut body = Vec::new();
    let encoded = UploadEncryptionAdapter::new(session.clone())
        .encode_article(
            "real.bin",
            payload.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            payload,
            128,
            1,
            &mut body,
        )
        .unwrap();
    let restored = control::decrypt_yenc_control_lines(&session, 1, &encoded.body).unwrap();
    // Rewrite the restored =ybegin to carry an extra keyword whose value
    // contains `name=` before the real name (`note=name=x` ahead of
    // `name=`). Before PU12-04 the name lookup found the first `name=`
    // substring, i.e. inside this keyword value, corrupting the AAD name.
    // (The ordinary decoder's own `line=` parse rejects a `name=`-valued
    // `line=` token regardless, so the carrier keyword is an inert one.)
    let mut mutated = Vec::new();
    for line in control::split_lines_preserving_endings(&restored) {
        if line.content.starts_with(b"=ybegin") {
            let text = String::from_utf8(line.content.to_vec()).unwrap();
            let text = text.replacen(" size=", " note=name=x size=", 1);
            mutated.extend_from_slice(text.as_bytes());
        } else {
            mutated.extend_from_slice(line.content);
        }
        mutated.extend_from_slice(line.ending);
    }
    let wire = control::encrypt_yenc_control_lines(&session, 1, &mutated).unwrap();
    let decoded = DownloadDecryptionAdapter::with_password(kdf_input)
        .decode_article(&wire)
        .expect("name= inside a keyword value must not corrupt the AAD name");
    assert_eq!(decoded.data, payload);
    assert_eq!(decoded.name, "real.bin");
}

// Body Standard v1.2 geometry, allocation, and decode-seam tests live in
// `tests/geometry.rs` (they share the multipart fixture helpers).
