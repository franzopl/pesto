//! Configuration regression tests, grouped by config domain.
//!
//! The production configuration module owns resolution, parsing, validation
//! and the TOML model. These suites follow the same boundaries so a change to
//! one domain only requires loading its focused tests.

use crate::config::*;

mod behavior;
mod cli_overrides;
mod defaults;
mod loading;
mod obfuscation;
mod parsing;
mod servers;
mod toml_sections;

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

fn base_overrides() -> Overrides {
    Overrides {
        groups: Some(vec!["alt.test".into()]),
        ..Default::default()
    }
}

fn minimal_file() -> FileConfig {
    let mut f = FileConfig::default();
    f.server.host = Some("h".into());
    f.posting.groups = Some(vec!["alt.test".into()]);
    f
}
