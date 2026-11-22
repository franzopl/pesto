use proptest::prelude::*;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

use super::adapter::{parse_yencryption_line, DownloadDecryptionAdapter, UploadEncryptionAdapter};
use super::body::{decrypt_body, encrypt_body, format_body_aad};
use super::control::{self, extract_bootstrap_from_line1, ff1_decrypt_line, ff1_encrypt_line};
use super::kdf::EncryptionSession;
use crate::yenc::PartSpec;

fn bytes_from_value(value: &serde_json::Value) -> Vec<u8> {
    value
        .as_array()
        .expect("fixture bytes array")
        .iter()
        .map(|b| b.as_u64().expect("byte value") as u8)
        .collect()
}
fn fixture_bytes(case: &serde_json::Value, name: &str) -> Vec<u8> {
    bytes_from_value(&case[format!("{name}_bytes")])
}
fn fixture_bytes_list(case: &serde_json::Value, name: &str) -> Vec<Vec<u8>> {
    case[format!("{name}_bytes")]
        .as_array()
        .expect("fixture bytes list")
        .iter()
        .map(bytes_from_value)
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

#[derive(Deserialize)]
struct Argon2idFixture {
    vectors: Vec<Argon2idVector>,
}

#[derive(Deserialize)]
struct Argon2idVector {
    id: String,
    #[serde(alias = "password")]
    transport_kdf_input: String,
    salt_bytes: Vec<u8>,
    expected_key_bytes: Vec<u8>,
    #[serde(default)]
    no_normalization_distinct_passwords: Option<Vec<String>>,
}

#[test]
fn test_argon2id_kdf_test_vectors() {
    let path = test_vectors_dir().join("argon2id.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: Argon2idFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.vectors {
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&vec.salt_bytes);

        let session = EncryptionSession::new(&vec.transport_kdf_input, salt)
            .unwrap_or_else(|e| panic!("session creation failed for vector {}: {e}", vec.id));

        assert_eq!(
            session.master_key().to_vec(),
            vec.expected_key_bytes,
            "master key mismatch for vector {}",
            vec.id
        );

        if let Some(triplet) = &vec.no_normalization_distinct_passwords {
            assert_eq!(triplet.len(), 3, "vector {} triplet size", vec.id);
            let mut keys: Vec<Vec<u8>> = Vec::new();
            for password in triplet {
                let s = EncryptionSession::new(password, salt)
                    .unwrap_or_else(|e| panic!("session failed for vector {}: {e}", vec.id));
                keys.push(s.master_key().to_vec());
            }
            assert_ne!(
                keys[0], keys[1],
                "vector {}: triplet keys 0/1 must differ",
                vec.id
            );
            assert_ne!(
                keys[0], keys[2],
                "vector {}: triplet keys 0/2 must differ",
                vec.id
            );
            assert_ne!(
                keys[1], keys[2],
                "vector {}: triplet keys 1/2 must differ",
                vec.id
            );
        }
    }
}

#[derive(Deserialize)]
struct NonceTweakFixture {
    body_nonce_vectors: Vec<BodyNonceVector>,
    control_tweak_vectors: Vec<ControlTweakVector>,
}

#[derive(Deserialize)]
struct BodyNonceVector {
    id: String,
    key_bytes: Vec<u8>,
    segment_index: u32,
    expected_nonce_bytes: Vec<u8>,
}

#[derive(Deserialize)]
struct ControlTweakVector {
    id: String,
    master_key_bytes: Vec<u8>,
    segment_index: u32,
    line_index: u32,
    expected_tweak_bytes: Vec<u8>,
}

#[test]
fn test_nonce_and_tweak_test_vectors() {
    let path = test_vectors_dir().join("nonce_tweak.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: NonceTweakFixture = serde_json::from_str(&content).unwrap();

    // The body-nonce vectors all share one master key (the Argon2id key
    // for the published vector password / salt `1a2b…`, verified against argon2id.json by
    // `test_argon2id_kdf_test_vectors`). Anchor a production session on
    // that pair and verify every vector through the production
    // derivation instead of re-implementing the HMAC here.
    let anchor = fixture
        .body_nonce_vectors
        .first()
        .expect("body nonce vectors must be present");
    let anchor_key = anchor.key_bytes.clone();
    let argon2id_path = test_vectors_dir().join("argon2id.json");
    let argon2id_content = std::fs::read_to_string(&argon2id_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", argon2id_path.display()));
    let argon2id_fixture: Argon2idFixture = serde_json::from_str(&argon2id_content).unwrap();
    let anchor_vec = argon2id_fixture
        .vectors
        .iter()
        .find(|v| v.expected_key_bytes == anchor_key)
        .expect("body nonce key must be an argon2id vector key");
    let mut anchor_salt = [0u8; 16];
    anchor_salt.copy_from_slice(&anchor_vec.salt_bytes);
    let body_session =
        EncryptionSession::new(&anchor_vec.transport_kdf_input, anchor_salt).unwrap();
    assert_eq!(
        body_session.master_key().to_vec(),
        anchor.key_bytes,
        "body nonce session must carry the vectors' master key"
    );
    for vec in &fixture.body_nonce_vectors {
        assert_eq!(
            vec.key_bytes, anchor.key_bytes,
            "body nonce vector {} must share the anchored key",
            vec.id
        );
        let nonce = body_session.derive_body_nonce(vec.segment_index);
        assert_eq!(
            nonce.to_vec(),
            vec.expected_nonce_bytes,
            "body nonce mismatch for vector {}",
            vec.id
        );
    }

    assert!(
        !fixture.control_tweak_vectors.is_empty(),
        "control tweak vectors must be present"
    );
    // v1.2 nonce_tweak.json no longer carries a
    // `control_tweak_argon2id_vector` reference; the control tweak vectors
    // carry `master_key_bytes` directly. Anchor the Argon2id session on the
    // first control tweak vector's segment/line setup via its referenced
    // control_line_encryption vector instead.
    let control_path = test_vectors_dir().join("control_line_encryption.json");
    let control_content = std::fs::read_to_string(&control_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", control_path.display()));
    let control_fixture: ControlLineFixture = serde_json::from_str(&control_content).unwrap();
    let control_vector = control_fixture
        .vectors
        .iter()
        .find(|vector| vector["id"] == "control-vec-01-line-1-ybegin-single")
        .expect("canonical control-line vector must exist");
    // Rebuild the Argon2id session from the control vector's own password
    // and salt (v1.2 nonce_tweak.json carries no Argon2id back-reference).
    let session_salt_bytes = fixture_bytes(control_vector, "salt");
    let mut session_salt = [0u8; 16];
    session_salt.copy_from_slice(&session_salt_bytes);
    let session = EncryptionSession::new(
        control_vector
            .get("transport_kdf_input")
            .or_else(|| control_vector.get("password"))
            .unwrap()
            .as_str()
            .unwrap(),
        session_salt,
    )
    .unwrap();
    // Cross-check: the control vector's password/salt must match an
    // Argon2id vector (same password → same derived master key).
    let control_password = control_vector
        .get("transport_kdf_input")
        .or_else(|| control_vector.get("password"))
        .unwrap()
        .as_str()
        .unwrap();
    let matching_argon2id = argon2id_fixture
        .vectors
        .iter()
        .find(|v| {
            v.transport_kdf_input == control_password
                && v.salt_bytes == fixture_bytes(control_vector, "salt")
        })
        .expect("control vector password+salt must reference an argon2id vector");
    assert_eq!(
        session.master_key().to_vec(),
        matching_argon2id.expected_key_bytes,
        "session master key must match the referenced argon2id vector"
    );

    assert_eq!(
        session.control_key().to_vec(),
        fixture_bytes(control_vector, "derived_enc_key"),
        "derived control key mismatch"
    );

    // Every tweak vector must name the anchored session's master key;
    // that is what makes the production-derivation assertions below
    // meaningful for these vectors.
    for vec in &fixture.control_tweak_vectors {
        assert_eq!(
            vec.master_key_bytes,
            session.master_key().to_vec(),
            "control tweak vector {} must share the anchored master key",
            vec.id
        );
    }

    for vec in &fixture.control_tweak_vectors {
        let tweak = session.derive_control_tweak(vec.segment_index, vec.line_index);
        assert_eq!(
            tweak.to_vec(),
            vec.expected_tweak_bytes,
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
    salt_bytes: Vec<u8>,
    segment_index: u32,
    derived_key_bytes: Vec<u8>,
    derived_nonce_bytes: Vec<u8>,
    #[serde(default)]
    aad: Option<AadVector>,
    plaintext_bytes: Vec<u8>,
    expected_ciphertext_bytes: Vec<u8>,
    #[serde(default)]
    expected_index_bytes: Option<Vec<u8>>,
    expected_tag_bytes: Vec<u8>,
    #[serde(default)]
    expected_yencryption_line: Option<String>,
}

#[derive(Deserialize)]
struct AadVector {
    expected_aad_bytes: Vec<u8>,
    part_number: u32,
    part_begin: u32,
    segment_bytes: u64,
    name_bytes_bytes: Vec<u8>,
}

#[test]
fn test_body_encryption_test_vectors() {
    let path = test_vectors_dir().join("body_encryption.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: BodyEncryptionFixture = serde_json::from_str(&content).unwrap();

    for vec in &fixture.vectors {
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&vec.salt_bytes);

        let session = EncryptionSession::new(&vec.transport_kdf_input, salt).unwrap();
        assert_eq!(
            session.master_key().to_vec(),
            vec.derived_key_bytes,
            "derived key mismatch for vector {}",
            vec.id
        );
        let nonce = session.derive_body_nonce(vec.segment_index);
        assert_eq!(
            nonce.to_vec(),
            vec.derived_nonce_bytes,
            "derived nonce mismatch for vector {}",
            vec.id
        );

        let plaintext = vec.plaintext_bytes.clone();
        // Deserialize the AAD inputs and compute the AAD, validating
        // `format_body_aad` against the vector's expected serialization.
        let aad = match &vec.aad {
            Some(a) => {
                let name = a.name_bytes_bytes.clone();
                let computed = format_body_aad(a.part_number, a.part_begin, a.segment_bytes, &name);
                assert_eq!(
                    computed, a.expected_aad_bytes,
                    "AAD mismatch for vector {}",
                    vec.id
                );
                computed
            }
            None => Vec::new(),
        };
        let (ct, tag) = encrypt_body(&plaintext, &aad, session.master_key(), &nonce)
            .unwrap_or_else(|e| panic!("encrypt failed for vector {}: {e}", vec.id));

        assert_eq!(
            ct, vec.expected_ciphertext_bytes,
            "ciphertext mismatch for vector {}",
            vec.id
        );
        assert_eq!(
            tag.to_vec(),
            vec.expected_tag_bytes,
            "tag mismatch for vector {}",
            vec.id
        );

        if let Some(expected_index) = &vec.expected_index_bytes {
            assert_eq!(
                vec.segment_index.to_be_bytes().to_vec(),
                *expected_index,
                "index bytes mismatch for vector {}",
                vec.id
            );
        }

        if let Some(expected_line) = &vec.expected_yencryption_line {
            let actual_line = format!(
                "=yencryption cipher=XChaCha20-Poly1305 salt={} index={:08x} tag={}",
                hex_encode(&vec.salt_bytes),
                vec.segment_index,
                hex_encode(&tag)
            );
            assert_eq!(
                actual_line, *expected_line,
                "header mismatch for vector {}",
                vec.id
            );
        }

        // Roundtrip decrypt
        let decrypted = decrypt_body(&ct, &tag, &aad, session.master_key(), &nonce)
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

mod control_vectors;
mod malformed;
