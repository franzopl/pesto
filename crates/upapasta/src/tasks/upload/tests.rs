//! Focused unit tests for the season-pack `<head>` metadata helper in
//! [`super::season_nzb_meta`] (PESTO-R6-2). Kept as a sibling `#[cfg(test)]`
//! module file so `tasks/upload.rs` stays under the source-size limit while
//! the seam stays directly testable without starting the TUI.

use super::season_nzb_meta;
use pesto::config::{Config, ObfuscateMode};

fn fixture_kdf_input() -> &'static str {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../pesto/test-vectors/body_encryption.json");
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

fn base_config() -> Config {
    Config {
        host: "unused".into(),
        port: 563,
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        retry_delay: 0,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        proxy: None,
        proxy_check_ip: false,
        extra_servers: vec![],
        from: "tester <t@pesto.test>".into(),
        groups: vec!["alt.binaries.test".into()],
        article_size: 768_000,
        line_length: 128,
        retries: 1,
        obfuscate: ObfuscateMode::None,
        date: None,
        no_archive: false,
        file_counter: false,
        message_id_domain: None,
        dry_run: false,
        par2: 0,
        par2_effort: 1.0,
        par2_memory_limit: None,
        memory_limit: None,
        par2_temp_dir: None,
        compress_temp_dir: None,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_only: false,
        par2_before_upload: false,
        threads: 0,
        simd: parmesan::SimdPath::Auto,
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        compress_volume_size: None,
        nzb_title: None,
        nzb_password: None,
        encrypt_password: None,
        ext: Vec::new(),
        exclude: Vec::new(),
        no_exclude: false,
        nzb_category: None,
        nzb_tags: Vec::new(),
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
        tvdb_kind: None,
        mal_id: None,
        nzb_dir: None,
        indexer_url: None,
        indexer_api_key: None,
        history: false,
        history_dir: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: true,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 0,
        check_retries: 1,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        check_recover_percent: 15,
        check_recover_max: 0,
        pipeline_depth: 0,
        keepalive_interval: 60,
    }
}

/// Body Std v1.2 §8: an encrypted season config must mark the pack and
/// carry the transport password with the artifacts.rs precedence —
/// encryption password wins over both the NZB and compression fallbacks.
#[test]
fn encrypted_config_marks_pack_and_prefers_encryption_password() {
    let mut config = base_config();
    config.encrypt_password = Some(fixture_kdf_input().to_string());
    config.nzb_password = Some("conformance-fixture-nzb-password".into());
    config.compress_password = Some("conformance-fixture-archive-password".into());

    let meta = season_nzb_meta(&config, "Show.S01".into());

    assert!(meta.yenc_encrypted);
    assert_eq!(meta.password.as_deref(), Some(fixture_kdf_input()));
    assert_eq!(meta.name.as_deref(), Some("Show.S01"));
}

/// With `--encrypt` absent the pack stays ordinary: no marker, and the
/// existing nzb-then-compression password fallback is preserved
/// (including discarding the archive password only when no fallback
/// applies is NOT the behaviour here — compression still wins last).
#[test]
fn unencrypted_config_keeps_existing_password_fallback_order() {
    let mut config = base_config();
    config.nzb_password = Some("conformance-fixture-nzb-password".into());
    config.compress_password = Some("conformance-fixture-archive-password".into());

    let meta = season_nzb_meta(&config, "Show.S01".into());

    assert!(!meta.yenc_encrypted);
    assert_eq!(
        meta.password.as_deref(),
        Some("conformance-fixture-nzb-password")
    );
}

/// Neither NZB nor encryption password: the compression/archive
/// fallback is retained exactly as before the encryption work.
#[test]
fn unencrypted_config_falls_back_to_compression_password() {
    let mut config = base_config();
    config.compress_password = Some("conformance-fixture-archive-password".into());

    let meta = season_nzb_meta(&config, "Show.S01".into());

    assert!(!meta.yenc_encrypted);
    assert_eq!(
        meta.password.as_deref(),
        Some("conformance-fixture-archive-password")
    );
}

/// A config with an NZB password but no encryption must not suddenly
/// become an encrypted pack — the marker is tied to the encryption
/// password alone.
#[test]
fn nzb_password_alone_never_marks_the_pack_encrypted() {
    let mut config = base_config();
    config.nzb_password = Some("conformance-fixture-nzb-password".into());

    let meta = season_nzb_meta(&config, "Show.S01".into());

    assert!(!meta.yenc_encrypted);
    assert_eq!(
        meta.password.as_deref(),
        Some("conformance-fixture-nzb-password")
    );
}

/// With no passwords at all the metadata stays plain (empty password,
/// unmarked), matching the ordinary season output.
#[test]
fn passwordless_config_stays_plain() {
    let meta = season_nzb_meta(&base_config(), "Show.S01".into());

    assert!(!meta.yenc_encrypted);
    assert_eq!(meta.password, None);
}
