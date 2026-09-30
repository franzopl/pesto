use hmac::Mac;
use proptest::prelude::*;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

use super::adapter::{
    extract_and_remove_yencryption, parse_yencryption_line, DownloadDecryptionAdapter,
    UploadEncryptionAdapter,
};
use super::body::{decrypt_body, encrypt_body};
use super::control::{self, extract_salt_from_line1, ff1_decrypt_line, ff1_encrypt_line};
use super::kdf::EncryptionSession;
use crate::poster::SegmentIdentity;
use crate::yenc::PartSpec;

fn hex_decode(s: &str) -> Vec<u8> {
    let s = s.trim();
    assert!(s.len().is_multiple_of(2), "hex string length must be even");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

fn test_vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-vectors")
}

#[derive(Deserialize)]
struct Argon2idFixture {
    vectors: Vec<Argon2idVector>,
}

#[derive(Deserialize)]
struct Argon2idVector {
    id: String,
    #[serde(alias = "password")]
    transport_kdf_input: String,
    salt_hex: String,
    expected_key_hex: String,
}

#[test]
fn test_argon2id_kdf_test_vectors() {
    let path = test_vectors_dir().join("argon2id.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: Argon2idFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.vectors {
        let salt_bytes = hex_decode(&vec.salt_hex);
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&salt_bytes);

        let session = EncryptionSession::new(&vec.transport_kdf_input, salt)
            .unwrap_or_else(|e| panic!("session creation failed for vector {}: {e}", vec.id));

        let derived_hex = hex_encode(session.master_key());
        assert_eq!(
            derived_hex, vec.expected_key_hex,
            "master key mismatch for vector {}",
            vec.id
        );
    }
}

#[derive(Deserialize)]
struct NonceTweakFixture {
    body_nonce_vectors: Vec<BodyNonceVector>,
    control_tweak_argon2id_vector: String,
    control_tweak_vectors: Vec<ControlTweakVector>,
}

#[derive(Deserialize)]
struct BodyNonceVector {
    id: String,
    key_hex: String,
    segment_index: u32,
    expected_nonce_hex: String,
}

#[derive(Deserialize)]
struct ControlTweakVector {
    id: String,
    segment_index: u32,
    line_index: u32,
    full_hmac_hex: String,
    expected_tweak_hex: String,
}

#[test]
fn test_nonce_and_tweak_test_vectors() {
    let path = test_vectors_dir().join("nonce_tweak.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: NonceTweakFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.body_nonce_vectors {
        let key_bytes = hex_decode(&vec.key_hex);
        let mut master_key = [0u8; 32];
        master_key.copy_from_slice(&key_bytes);

        // Derive nonce directly via HMAC-SHA256
        let mut mac = <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(&master_key).unwrap();
        mac.update(b"yenc-body nonce");
        mac.update(&vec.segment_index.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let nonce_hex = hex_encode(&digest[0..24]);

        assert_eq!(
            nonce_hex, vec.expected_nonce_hex,
            "body nonce mismatch for vector {}",
            vec.id
        );
    }

    let argon2id_path = test_vectors_dir().join("argon2id.json");
    let argon2id_content = std::fs::read_to_string(&argon2id_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", argon2id_path.display()));
    let argon2id_fixture: Argon2idFixture = serde_json::from_str(&argon2id_content).unwrap();
    let argon2id_vector = argon2id_fixture
        .vectors
        .iter()
        .find(|vector| vector.id == fixture.control_tweak_argon2id_vector)
        .expect("referenced Argon2id vector must exist");
    let salt_bytes = hex_decode(&argon2id_vector.salt_hex);
    let mut salt = [0u8; 16];
    salt.copy_from_slice(&salt_bytes);
    let session = EncryptionSession::new(&argon2id_vector.transport_kdf_input, salt).unwrap();
    assert_eq!(
        hex_encode(session.master_key()),
        argon2id_vector.expected_key_hex,
        "referenced Argon2id master key mismatch"
    );

    let control_path = test_vectors_dir().join("control_line_encryption.json");
    let control_content = std::fs::read_to_string(&control_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", control_path.display()));
    let control_fixture: ControlLineFixture = serde_json::from_str(&control_content).unwrap();
    let control_vector = control_fixture
        .vectors
        .iter()
        .find(|vector| vector["id"] == "control-vec-01-line-1-ybegin-single")
        .expect("canonical control-line vector must exist");
    assert_eq!(
        control_vector["transport_kdf_input"].as_str().unwrap(),
        argon2id_vector.transport_kdf_input
    );
    assert_eq!(
        control_vector["salt_hex"].as_str().unwrap(),
        argon2id_vector.salt_hex
    );

    let mut key_mac =
        <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(session.master_key()).unwrap();
    key_mac.update(b"yenc-control key");
    let enc_key = key_mac.finalize().into_bytes();
    assert_eq!(
        hex_encode(&enc_key),
        control_vector["derived_enc_key_hex"].as_str().unwrap(),
        "derived control key mismatch"
    );

    for vec in &fixture.control_tweak_vectors {
        let mut tweak_mac =
            <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(session.master_key()).unwrap();
        tweak_mac.update(b"yenc-control tweak");
        tweak_mac.update(&vec.segment_index.to_be_bytes());
        tweak_mac.update(&vec.line_index.to_be_bytes());
        let tweak = tweak_mac.finalize().into_bytes();
        assert_eq!(
            hex_encode(&tweak),
            vec.full_hmac_hex,
            "control HMAC mismatch for vector {}",
            vec.id
        );
        assert_eq!(
            hex_encode(&tweak[0..8]),
            vec.expected_tweak_hex,
            "control tweak mismatch for vector {}",
            vec.id
        );
    }
}

#[derive(Deserialize)]
struct BodyEncryptionFixture {
    vectors: Vec<BodyEncryptionVector>,
}

#[derive(Deserialize)]
struct BodyEncryptionVector {
    id: String,
    #[serde(alias = "password")]
    transport_kdf_input: String,
    salt_hex: String,
    segment_index: u32,
    plaintext_hex: String,
    expected_ciphertext_hex: String,
    expected_tag_hex: String,
}

#[test]
fn test_body_encryption_test_vectors() {
    let path = test_vectors_dir().join("body_encryption.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: BodyEncryptionFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.vectors {
        let salt_bytes = hex_decode(&vec.salt_hex);
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&salt_bytes);

        let session = EncryptionSession::new(&vec.transport_kdf_input, salt).unwrap();
        let nonce = session.derive_body_nonce(vec.segment_index);

        let plaintext = hex_decode(&vec.plaintext_hex);
        let (ct, tag) = encrypt_body(&plaintext, session.master_key(), &nonce)
            .unwrap_or_else(|e| panic!("encrypt failed for vector {}: {e}", vec.id));

        assert_eq!(
            hex_encode(&ct),
            vec.expected_ciphertext_hex,
            "ciphertext mismatch for vector {}",
            vec.id
        );
        assert_eq!(
            hex_encode(&tag),
            vec.expected_tag_hex,
            "tag mismatch for vector {}",
            vec.id
        );

        // Roundtrip decrypt
        let decrypted = decrypt_body(&ct, &tag, session.master_key(), &nonce)
            .unwrap_or_else(|e| panic!("decrypt failed for vector {}: {e}", vec.id));
        assert_eq!(
            decrypted, plaintext,
            "decrypted plaintext mismatch for vector {}",
            vec.id
        );
    }
}

#[derive(Deserialize)]
struct ControlLineFixture {
    vectors: Vec<serde_json::Value>,
}

#[test]
fn test_control_line_encryption_test_vectors() {
    let path = test_vectors_dir().join("control_line_encryption.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: ControlLineFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.vectors {
        let id = vec["id"].as_str().unwrap();

        if let Some(plaintext_line) = vec.get("plaintext_line").and_then(|v| v.as_str()) {
            let enc_key_bytes = hex_decode(vec["derived_enc_key_hex"].as_str().unwrap());
            let mut enc_key = [0u8; 32];
            enc_key.copy_from_slice(&enc_key_bytes);

            let tweak_bytes = hex_decode(vec["derived_tweak_hex"].as_str().unwrap());
            let mut tweak = [0u8; 8];
            tweak.copy_from_slice(&tweak_bytes);

            let pt_bytes = plaintext_line.as_bytes();
            let ct_bytes = ff1_encrypt_line(&enc_key, &tweak, pt_bytes)
                .unwrap_or_else(|e| panic!("ff1 encrypt failed for vector {id}: {e}"));

            let is_line_1 = vec["is_line_1"].as_bool().unwrap_or(false);
            let wire_bytes = if is_line_1 {
                let expected_wire = hex_decode(vec["expected_wire_hex"].as_str().unwrap());
                let salt = &expected_wire[0..16];
                [salt, &ct_bytes].concat()
            } else {
                ct_bytes.clone()
            };

            let expected_wire_hex = vec["expected_wire_hex"].as_str().unwrap();
            assert_eq!(
                hex_encode(&wire_bytes),
                expected_wire_hex,
                "control line wire mismatch for vector {id}"
            );

            // Decrypt
            let restored_pt = ff1_decrypt_line(&enc_key, &tweak, &ct_bytes)
                .unwrap_or_else(|e| panic!("ff1 decrypt failed for vector {id}: {e}"));
            assert_eq!(
                restored_pt, pt_bytes,
                "control line roundtrip mismatch for vector {id}"
            );
        } else if let Some(input_lines_val) = vec.get("input_lines").and_then(|v| v.as_array()) {
            let lines: Vec<&str> = input_lines_val
                .iter()
                .map(|l| l.as_str().unwrap())
                .collect();
            let expected_wire_hexes: Vec<&str> = vec["expected_wire_lines_hex"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h.as_str().unwrap())
                .collect();

            let password = vec["transport_kdf_input"].as_str().unwrap();
            let salt_bytes = hex_decode(vec["salt_hex"].as_str().unwrap());
            let mut salt = [0u8; 16];
            salt.copy_from_slice(&salt_bytes);
            let session = EncryptionSession::new(password, salt).unwrap();
            let segment_index = vec["segment_index"].as_u64().unwrap() as u32;

            let joined = lines.join("\r\n") + "\r\n";
            let encrypted_wire =
                control::encrypt_yenc_control_lines(&session, segment_index, joined.as_bytes())
                    .unwrap_or_else(|e| panic!("encrypt_yenc_control_lines failed for {id}: {e}"));

            let split_wire = control::split_lines_preserving_endings(&encrypted_wire);
            assert_eq!(
                split_wire.len(),
                expected_wire_hexes.len(),
                "line count mismatch for {id}"
            );
            for (actual, expected_hex) in split_wire.iter().zip(&expected_wire_hexes) {
                assert_eq!(
                    hex_encode(actual.content),
                    *expected_hex,
                    "wire line hex mismatch in {id}"
                );
            }

            let decrypted =
                control::decrypt_yenc_control_lines(&session, segment_index, &encrypted_wire)
                    .unwrap_or_else(|e| panic!("decrypt_yenc_control_lines failed for {id}: {e}"));
            assert_eq!(
                decrypted,
                joined.as_bytes(),
                "full article roundtrip failed for {id}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn control_line_roundtrip_preserves_framing_for_arbitrary_session_identity(
        password in "[ -~]{1,32}",
        salt_numerals in prop::array::uniform16(0u16..253),
        segment_index in 1u32..=u32::MAX,
        name in "[A-Za-z0-9._-]{1,48}",
        payload in prop::collection::vec(any::<u8>(), 0..512),
        line_len in 1usize..129,
    ) {
        let mut salt = [0u8; 16];
        for (byte, numeral) in salt.iter_mut().zip(salt_numerals) {
            *byte = control::numeral_to_byte(numeral).unwrap();
        }
        let session = EncryptionSession::new(&password, salt).unwrap();
        let mut yenc = Vec::new();
        let spec = PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        };
        let encoded = crate::yenc::encode_part_into(
            &name,
            payload.len().max(1) as u64,
            spec,
            &payload,
            line_len,
            None,
            &mut yenc,
        );
        let insertion = encoded.body
            .windows(7)
            .position(|window| window == b"\n=ypart")
            .map(|position| position + 1)
            .unwrap();
        let insertion = insertion
            + encoded.body[insertion..]
                .iter()
                .position(|&byte| byte == b'\n')
                .unwrap()
            + 1;
        let encryption_line = format!(
            "=yencryption cipher=XChaCha20-Poly1305 salt={} tag=00000000000000000000000000000000\r\n",
            hex_encode(&salt),
        );
        let mut plaintext = Vec::new();
        plaintext.extend_from_slice(&encoded.body[..insertion]);
        plaintext.extend_from_slice(encryption_line.as_bytes());
        plaintext.extend_from_slice(&encoded.body[insertion..]);

        let wire = control::encrypt_yenc_control_lines(&session, segment_index, &plaintext).unwrap();
        let restored = control::decrypt_yenc_control_lines(&session, segment_index, &wire).unwrap();

        prop_assert_eq!(restored, plaintext);
    }
}

#[derive(Deserialize)]
struct MalformedFixture {
    vectors: Vec<serde_json::Value>,
}

#[test]
fn test_malformed_inputs_rejection() {
    let path = test_vectors_dir().join("malformed_inputs.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: MalformedFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.vectors {
        let id = vec["id"].as_str().unwrap();
        let category = vec["category"].as_str().unwrap();
        let expected_err = vec["expected_error"].as_str().unwrap();

        match category {
            "header_syntax" => {
                let input_line = vec["input_line"].as_str().unwrap();
                let res = parse_yencryption_line(input_line.as_bytes());
                assert!(
                    res.is_err(),
                    "vector {id} expected error {expected_err}, but passed"
                );
                let err_msg = res.unwrap_err().to_string();
                assert!(
                    err_msg.contains(expected_err),
                    "vector {id} error message '{err_msg}' should contain '{expected_err}'"
                );
            }
            "auth_failure" => {
                let password = vec["transport_kdf_input"].as_str().unwrap();
                let salt_bytes = hex_decode(vec["salt_hex"].as_str().unwrap());
                let mut salt = [0u8; 16];
                salt.copy_from_slice(&salt_bytes);
                let session = EncryptionSession::new(password, salt).unwrap();

                let ct_hex = vec
                    .get("tampered_ciphertext_hex")
                    .or_else(|| vec.get("ciphertext_hex"))
                    .unwrap()
                    .as_str()
                    .unwrap();
                let ct = hex_decode(ct_hex);

                let tag_hex = vec
                    .get("tampered_tag_hex")
                    .or_else(|| vec.get("tag_hex"))
                    .unwrap()
                    .as_str()
                    .unwrap();
                let tag_bytes = hex_decode(tag_hex);
                let mut tag = [0u8; 16];
                tag.copy_from_slice(&tag_bytes);

                let segment_index = vec["segment_index"].as_u64().unwrap() as u32;
                let nonce = session.derive_body_nonce(segment_index);

                let res = decrypt_body(&ct, &tag, session.master_key(), &nonce);
                assert!(
                    res.is_err(),
                    "vector {id} expected error {expected_err}, but passed"
                );
            }
            "control_syntax" => {
                if let Some(salt_hex) = vec.get("tampered_salt_hex").and_then(|v| v.as_str()) {
                    let salt_bytes = hex_decode(salt_hex);
                    // construct a line1 with 16-byte salt + minimal content
                    let line1 = [salt_bytes.as_slice(), b"=ybegin line=128 size=18"].concat();
                    let res = extract_salt_from_line1(&line1);
                    assert!(res.is_err(), "vector {id} expected error {expected_err}");
                    let err_msg = res.unwrap_err().to_string();
                    assert!(
                        err_msg.contains(expected_err),
                        "vector {id} error '{err_msg}' should contain '{expected_err}'"
                    );
                } else if let Some(line_hex) = vec.get("line_hex").and_then(|v| v.as_str()) {
                    let bytes = hex_decode(line_hex);
                    let dummy_key = [1u8; 32];
                    let dummy_tweak = [2u8; 8];
                    let res = ff1_encrypt_line(&dummy_key, &dummy_tweak, &bytes);
                    assert!(res.is_err(), "vector {id} expected error {expected_err}");
                    let err_msg = res.unwrap_err().to_string();
                    assert!(
                        err_msg.contains(expected_err),
                        "vector {id} error '{err_msg}' should contain '{expected_err}'"
                    );
                } else if let Some(line1_hex) = vec.get("line1_hex").and_then(|v| v.as_str()) {
                    let bytes = hex_decode(line1_hex);
                    let res = extract_salt_from_line1(&bytes);
                    assert!(res.is_err(), "vector {id} expected error {expected_err}");
                    let err_msg = res.unwrap_err().to_string();
                    assert!(
                        err_msg.contains(expected_err),
                        "vector {id} error '{err_msg}' should contain '{expected_err}'"
                    );
                } else if let Some(wrong_pwd) = vec.get("wrong_password").and_then(|v| v.as_str()) {
                    // Try to decrypt with wrong password
                    let salt = [0x42u8; 16];
                    let correct_session = EncryptionSession::new("test123", salt).unwrap();
                    let wrong_session = EncryptionSession::new(wrong_pwd, salt).unwrap();
                    let input = b"=ybegin line=128 size=18 name=file.bin\r\n=yend size=18\r\n";
                    let wire =
                        control::encrypt_yenc_control_lines(&correct_session, 1, input).unwrap();
                    let res = control::decrypt_yenc_control_lines(&wrong_session, 1, &wire);
                    assert!(res.is_err(), "vector {id} expected error {expected_err}");
                    let err_msg = res.unwrap_err().to_string();
                    assert!(
                        err_msg.contains(expected_err),
                        "vector {id} error '{err_msg}' should contain '{expected_err}'"
                    );
                }
            }
            _ => {}
        }
    }
}

#[test]
fn test_adapter_contracts() {
    let salt = [0x42u8; 16];
    let session = Arc::new(EncryptionSession::new("test-password-123", salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Hello, encrypted Usenet world! 1234567890";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();

    let mut body = Vec::new();
    let encoded = upload_adapter
        .encode_article(
            "test.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            None,
            identity,
            &mut body,
        )
        .expect("encode_article failed");

    assert!(!encoded.body.is_empty());
    // Control line 1 starts with 16-byte salt, so not with "=ybegin"
    assert!(!encoded.body.starts_with(b"=ybegin"));

    let download_adapter = DownloadDecryptionAdapter::new(Some(session));
    let decoded = download_adapter
        .decode_article(&encoded.body, Some(identity.segment_index))
        .expect("decode_article failed");

    assert_eq!(decoded.data, payload);
    assert_eq!(decoded.name, "test.bin");
    assert_eq!(decoded.file_size, payload.len() as u64);
}

#[test]
fn test_adapter_multipart_roundtrip() {
    let salt = [0x99u8; 16];
    let session = Arc::new(EncryptionSession::new("multi-part-password", salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    let payload = vec![0xAB; 2048];
    let spec = PartSpec {
        number: 2,
        total: 5,
        offset: 1024,
    };
    let identity = SegmentIdentity::checked(0, 1, 5, 2).unwrap();

    let mut body = Vec::new();
    let encoded = upload_adapter
        .encode_article(
            "multi.bin",
            5120,
            spec,
            &payload,
            128,
            Some(0x12345678),
            identity,
            &mut body,
        )
        .expect("encode_article failed");

    assert_eq!(encoded.number, 2);
    assert_eq!(encoded.total, 5);

    let download_adapter = DownloadDecryptionAdapter::with_password("multi-part-password");
    let decoded = download_adapter
        .decode_article(&encoded.body, Some(identity.segment_index))
        .expect("decode_article failed");

    assert_eq!(decoded.data, payload);
    assert_eq!(decoded.name, "multi.bin");
    assert_eq!(decoded.part, 2);
    assert_eq!(decoded.total, 5);
    assert_eq!(decoded.file_size, 5120);
    assert_eq!(decoded.begin, 1025);
    assert_eq!(decoded.end, 3072);
}

#[test]
fn test_upload_adapter_encapsulation() {
    // 1. Unencrypted delegation (no adapter present)
    let payload = b"Plain unencrypted payload bytes 12345";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let mut unencrypted_buf = Vec::new();
    let unencrypted_part = crate::yenc::encode_part_into(
        "plain.bin",
        payload.len() as u64,
        spec,
        payload,
        128,
        None,
        &mut unencrypted_buf,
    );
    assert!(unencrypted_part.body.starts_with(b"=ybegin"));
    assert!(!unencrypted_part
        .body
        .windows(13)
        .any(|w| w == b"=yencryption "));

    // Verify unencrypted decode
    let plain_decoded = crate::yenc::decode_part(&unencrypted_part.body).unwrap();
    assert_eq!(plain_decoded.data, payload);

    // Also verify DownloadDecryptionAdapter passes unencrypted articles through untouched
    let download_adapter_empty = DownloadDecryptionAdapter::new(None);
    let pass_through_decoded = download_adapter_empty
        .decode_article(&unencrypted_part.body, None)
        .expect("unencrypted article should decode transparently without session");
    assert_eq!(pass_through_decoded.data, payload);

    // 2. Encrypted delegation (adapter present)
    let salt = [0x55u8; 16];
    let session = Arc::new(EncryptionSession::new("encapsulation-test-pass", salt).unwrap());
    let adapter = UploadEncryptionAdapter::new(session.clone());
    let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();

    let mut encrypted_buf = Vec::new();
    let encrypted_part = adapter
        .encode_article(
            "secret.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            None,
            identity,
            &mut encrypted_buf,
        )
        .expect("encrypted encode failed");

    // Must NOT start with =ybegin because line 1 has prepended salt and FF1 encryption
    assert!(!encrypted_part.body.starts_with(b"=ybegin"));
    // Payload on wire must be ciphertext, not plaintext
    assert!(!encrypted_part
        .body
        .windows(payload.len())
        .any(|w| w == payload));

    // 3. Transparent decryption recovers original plaintext
    let download_adapter = DownloadDecryptionAdapter::new(Some(session));
    let decrypted_part = download_adapter
        .decode_article(&encrypted_part.body, Some(identity.segment_index))
        .expect("transparent decryption failed");

    assert_eq!(decrypted_part.data, payload);
    assert_eq!(decrypted_part.name, "secret.bin");
}

#[test]
fn test_generate_alphabet_salt_properties() {
    for _ in 0..1000 {
        let salt = control::generate_alphabet_salt();
        for &b in &salt {
            assert!(b != 0x00 && b != 0x0A && b != 0x0D);
            assert!(control::byte_to_numeral(b).is_ok());
        }
        let mut line1 = Vec::new();
        line1.extend_from_slice(&salt);
        line1.extend_from_slice(b"=ybegin part=1\r\n");
        let extracted = extract_salt_from_line1(&line1).expect("extract salt should succeed");
        assert_eq!(extracted, salt);
    }
}

#[test]
fn test_adapter_strict_header_order() {
    let password = "strict-header-test-password";
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    // 1. Single-part article: line 1 = =ybegin, line 2 = =yencryption
    let payload = b"Strict single part header verification payload";
    let spec_single = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let identity_single = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
    let mut body_single = Vec::new();
    let enc_single = upload_adapter
        .encode_article(
            "single.bin",
            payload.len() as u64,
            spec_single,
            payload,
            128,
            None,
            identity_single,
            &mut body_single,
        )
        .unwrap();

    let download_adapter = DownloadDecryptionAdapter::with_password(password);
    let dec_single = download_adapter
        .decode_article(&enc_single.body, Some(identity_single.segment_index))
        .expect("single-part decode must succeed");
    assert_eq!(dec_single.data, payload);
    assert_eq!(dec_single.part_crc32, None);
    assert_eq!(dec_single.file_crc32, None);

    // 2. Multipart article: line 1 = =ybegin, line 2 = =ypart, line 3 = =yencryption
    let spec_multi = PartSpec {
        number: 1,
        total: 2,
        offset: 0,
    };
    let identity_multi = SegmentIdentity::checked(0, 1, 2, 1).unwrap();
    let mut body_multi = Vec::new();
    let enc_multi = upload_adapter
        .encode_article(
            "multi.bin",
            payload.len() as u64 * 2,
            spec_multi,
            payload,
            128,
            None,
            identity_multi,
            &mut body_multi,
        )
        .unwrap();

    let dec_multi = download_adapter
        .decode_article(&enc_multi.body, Some(identity_multi.segment_index))
        .expect("multi-part decode must succeed");
    assert_eq!(dec_multi.data, payload);
    assert_eq!(dec_multi.part_crc32, None);
    assert_eq!(dec_multi.file_crc32, None);
}

#[test]
fn test_adapter_ciphertext_crc_validation() {
    let password = "crc-validation-test-password";
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Ciphertext CRC validation payload 123456789";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
    let mut body = Vec::new();
    let enc = upload_adapter
        .encode_article(
            "test.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            None,
            identity,
            &mut body,
        )
        .unwrap();

    // Decrypting the intact article works
    let download_adapter = DownloadDecryptionAdapter::with_password(password);
    let dec = download_adapter
        .decode_article(&enc.body, Some(identity.segment_index))
        .expect("intact article decodes");
    assert_eq!(dec.data, payload);
    assert_eq!(dec.part_crc32, None);
    assert_eq!(dec.file_crc32, None);

    // Tamper with the =yend crc in the wire
    // Note: line 1, 2, and =yend are FF1 encrypted.
    // If we decrypt the control lines, tamper the yend crc, and re-encrypt, we can verify that ciphertext CRC mismatch fails before body decryption.
    let restored =
        control::decrypt_yenc_control_lines(&session, identity.segment_index, &enc.body).unwrap();
    let text = String::from_utf8_lossy(&restored);
    // Mutate the crc32= in =yend
    let tampered_text = if let Some(idx) = text.find("crc32=") {
        let mut t = text.into_owned();
        t.replace_range(idx + 6..idx + 7, "f");
        t
    } else {
        panic!("missing crc32 in yend");
    };
    let re_encrypted = control::encrypt_yenc_control_lines(
        &session,
        identity.segment_index,
        tampered_text.as_bytes(),
    )
    .unwrap();
    let err = download_adapter
        .decode_article(&re_encrypted, Some(identity.segment_index))
        .unwrap_err();
    assert!(
        err.to_string().contains("ciphertext CRC mismatch")
            || err.to_string().contains("CRC")
            || err.to_string().contains("crc")
    );
}

#[test]
fn test_adapter_malformed_headers_matrix() {
    // 1. Test parse_yencryption_line on various malformed lines
    let bad_cases = [
        ("=yencryption", "MISSING_CIPHER"),
        ("=yencryption cipher=AES-256-GCM salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ed70d238067735a20783df5e094ccafa", "UNSUPPORTED_CIPHER"),
        ("=yencryption cipher=XChaCha20-Poly1305 tag=ed70d238067735a20783df5e094ccafa salt=1a2b3c4d5e6f7890abcdef1234567890", "REORDERED_HEADER"),
        ("=yencryption salt=1a2b3c4d5e6f7890abcdef1234567890 cipher=XChaCha20-Poly1305 tag=ed70d238067735a20783df5e094ccafa", "REORDERED_HEADER"),
        ("=yencryption cipher=XChaCha20-Poly1305 salt=1A2B3C4D5E6F7890ABCDEF1234567890 tag=ed70d238067735a20783df5e094ccafa", "INVALID_SALT_HEX"),
        ("=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ED70D238067735A20783DF5E094CCAFA", "INVALID_TAG_HEX"),
        ("=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef12345678 tag=ed70d238067735a20783df5e094ccafa", "INVALID_SALT_LENGTH"),
        ("=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef123456789011 tag=ed70d238067735a20783df5e094ccafa", "INVALID_SALT_LENGTH"),
        ("=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ed70d238067735a20783df5e094cca", "INVALID_TAG_LENGTH"),
        ("=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ed70d238067735a20783df5e094ccafaaa", "INVALID_TAG_LENGTH"),
        ("=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ed70d238067735a20783df5e094ccafa extra=1", "EXTRA_PARAMETER"),
        ("=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ed70d238067735a20783df5e094ccafa", "DUPLICATE_PARAMETER"),
    ];

    for (line, expected_err) in bad_cases {
        let res = parse_yencryption_line(line.as_bytes());
        assert!(res.is_err(), "expected error for line: {line}");
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains(expected_err),
            "expected error containing '{expected_err}', got '{msg}' for line '{line}'"
        );
    }

    // 2. Misplaced and duplicate =yencryption in full article
    let article_duplicate = b"=ybegin line=128 size=4 name=test.bin\r\n=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ed70d238067735a20783df5e094ccafa\r\ntest\r\n=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ed70d238067735a20783df5e094ccafa\r\n=yend size=4 crc32=d87f7e0c\r\n".to_vec();
    let res_dup = extract_and_remove_yencryption(&article_duplicate);
    assert!(res_dup.is_err());
    assert!(res_dup
        .unwrap_err()
        .to_string()
        .contains("duplicate or misplaced"));

    // Multipart missing =ypart before =yencryption
    let article_bad_multipart = b"=ybegin part=1 total=2 line=128 size=8 name=test.bin\r\n=yencryption cipher=XChaCha20-Poly1305 salt=1a2b3c4d5e6f7890abcdef1234567890 tag=ed70d238067735a20783df5e094ccafa\r\n=ypart begin=1 end=4\r\ntest\r\n=yend size=4 part=1 pcrc32=d87f7e0c\r\n";
    let res_bad_mp = extract_and_remove_yencryption(article_bad_multipart);
    assert!(res_bad_mp.is_err());
}

#[test]
fn test_adapter_zero_output_rejection_matrix() {
    let password = "zero-output-rejection-password";
    let salt = control::generate_alphabet_salt();
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
    let upload_adapter = UploadEncryptionAdapter::new(session.clone());

    let payload = b"Zero output guarantee test payload 12345";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let identity = SegmentIdentity::checked(0, 1, 1, 1).unwrap();
    let mut body = Vec::new();
    let enc = upload_adapter
        .encode_article(
            "test.bin",
            payload.len() as u64,
            spec,
            payload,
            128,
            None,
            identity,
            &mut body,
        )
        .unwrap();

    let download_adapter = Arc::new(DownloadDecryptionAdapter::with_password(password));

    // 1. Missing segment identity on encrypted article fails closed
    assert!(download_adapter.decode_article(&enc.body, None).is_err());

    // 2. Segment index mismatch fails closed
    assert!(download_adapter.decode_article(&enc.body, Some(2)).is_err());

    // 3. Wrong password fails closed
    let wrong_password_adapter = DownloadDecryptionAdapter::with_password("wrong-password");
    assert!(wrong_password_adapter
        .decode_article(&enc.body, Some(identity.segment_index))
        .is_err());

    // 4. Truncated article body fails closed
    let half = &enc.body[..enc.body.len() / 2];
    assert!(download_adapter
        .decode_article(half, Some(identity.segment_index))
        .is_err());

    // 5. Salt mismatch between line 1 and =yencryption fails closed
    let restored =
        control::decrypt_yenc_control_lines(&session, identity.segment_index, &enc.body).unwrap();
    let text = String::from_utf8_lossy(&restored);
    if let Some(idx) = text.find("salt=") {
        let mut t = text.into_owned();
        // Change one char in salt
        let replacement = if &t[idx + 5..idx + 6] == "a" {
            "b"
        } else {
            "a"
        };
        t.replace_range(idx + 5..idx + 6, replacement);
        let re_enc =
            control::encrypt_yenc_control_lines(&session, identity.segment_index, t.as_bytes())
                .unwrap();
        let err = download_adapter
            .decode_article(&re_enc, Some(identity.segment_index))
            .unwrap_err();
        assert!(err.to_string().contains("salt mismatch"));
    }

    // 6. Concurrency & idempotency test
    let mut handles = Vec::new();
    for _ in 0..10 {
        let adapter_clone = download_adapter.clone();
        let wire = enc.body.clone();
        handles.push(std::thread::spawn(move || {
            let res = adapter_clone
                .decode_article(&wire, Some(identity.segment_index))
                .unwrap();
            assert_eq!(res.data, payload);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}
