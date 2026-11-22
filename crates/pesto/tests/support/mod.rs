//! Shared test helpers for the pesto integration suites.
#![allow(dead_code)]

pub fn fixture_kdf_input() -> &'static str {
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

pub fn other_kdf_input() -> &'static str {
    "incorrect-transport-kdf-for-tests"
}

pub fn fixture_kdf_input_bytes() -> Vec<u8> {
    fixture_kdf_input().as_bytes().to_vec()
}
