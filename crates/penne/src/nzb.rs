//! Loading `.nzb` files.
//!
//! Parsing itself is not reimplemented here — [`pesto::nzb::parse`] already
//! does it (it is the same format `pesto` writes when posting). This module
//! adds the download-side conveniences: reading from disk and summarizing.

use std::path::Path;

use anyhow::{bail, Context, Result};
use pesto::nzb::ParsedNzb;

/// Read and parse a `.nzb` file from disk.
pub fn load(path: &Path) -> Result<ParsedNzb> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    pesto::nzb::parse(&contents).with_context(|| format!("parsing {}", path.display()))
}

/// Construct a transport decryption adapter from NZB metadata.
///
/// Decryption is activated strictly when `meta.yenc_encrypted` is true.
/// If `meta.yenc_encrypted` is false (e.g. unencrypted NZBs or NZBs with an archive password only),
/// returns `Ok(None)`.
/// If `meta.yenc_encrypted` is true, requires `meta.password`.
/// The password is never printed in error messages.
pub fn download_decryptor(
    meta: &pesto::nzb::NzbMeta,
) -> Result<Option<std::sync::Arc<pesto::crypto::DownloadDecryptionAdapter>>> {
    download_decryptor_with_password_override(meta, None)
}

/// Construct a transport decryption adapter from NZB metadata, letting an
/// explicitly supplied password (CLI `--password`, DECISIONS 2026-10-16 Q1(a))
/// override `<meta type="password">` for transport decryption.
///
/// The `yenc_encrypted` marker remains the only gate: without it the override
/// is never used for transport and `Ok(None)` is returned, so ordinary NZBs
/// (including those carrying only an archive password) are unaffected. With
/// the marker, the explicit override wins when present (even if the meta
/// password is absent); otherwise `meta.password` is used. An empty *effective*
/// password is rejected; the override being an empty string therefore fails
/// closed instead of silently falling back to the meta password. The password
/// is never printed in error messages.
pub fn download_decryptor_with_password_override(
    meta: &pesto::nzb::NzbMeta,
    password_override: Option<&str>,
) -> Result<Option<std::sync::Arc<pesto::crypto::DownloadDecryptionAdapter>>> {
    if !meta.yenc_encrypted {
        return Ok(None);
    }
    let password = match password_override {
        Some(overridden) => overridden,
        None => meta
            .password
            .as_deref()
            .context("encrypted NZB is missing password metadata")?,
    };
    if password.is_empty() {
        bail!("encrypted NZB password is empty");
    }
    Ok(Some(std::sync::Arc::new(
        pesto::crypto::DownloadDecryptionAdapter::with_password(password),
    )))
}

/// Aggregate counts over a parsed `.nzb`, used for `penne info` and for the
/// pre-download summary printed before a download starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub files: usize,
    pub segments: usize,
    pub total_bytes: u64,
}

/// Compute a [`Summary`] over every segment in a parsed `.nzb`.
pub fn summarize(parsed: &ParsedNzb) -> Summary {
    let mut files = std::collections::HashSet::new();
    let mut total_bytes = 0u64;
    for seg in &parsed.segments {
        files.insert(&seg.file_name);
        total_bytes += seg.bytes;
    }
    Summary {
        files: files.len(),
        segments: parsed.segments.len(),
        total_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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

    #[test]
    fn summarize_counts_files_and_segments() {
        let groups = vec!["alt.test".to_string()];
        let segments = vec![
            pesto::poster::PostedSegment {
                file_name: "a.bin".into(),
                file_path: Path::new("a.bin").into(),
                subject_name: "a.bin".into(),
                wire_name: "a.bin".into(),
                wire_yenc_name: "a.bin".into(),
                file_size: 1000,
                part: 1,
                total: 2,
                message_id: "<a1@x>".into(),
                bytes: 500,
                from: "poster <p@x>".into(),
                date: (None, None),
                full_crc32: 0,
                server_idx: 0,
                file_index: 0,
                total_files: 0,
                segment_index: 0,
                plaintext_digest: None,
            },
            pesto::poster::PostedSegment {
                file_name: "a.bin".into(),
                file_path: Path::new("a.bin").into(),
                subject_name: "a.bin".into(),
                wire_name: "a.bin".into(),
                wire_yenc_name: "a.bin".into(),
                file_size: 1000,
                part: 2,
                total: 2,
                message_id: "<a2@x>".into(),
                bytes: 500,
                from: "poster <p@x>".into(),
                date: (None, None),
                full_crc32: 0,
                server_idx: 0,
                file_index: 0,
                total_files: 0,
                segment_index: 0,
                plaintext_digest: None,
            },
        ];
        let xml = pesto::nzb::generate(
            &groups,
            &segments,
            &pesto::nzb::NzbMeta::default(),
            pesto::config::ObfuscateMode::None,
        );

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(xml.as_bytes()).unwrap();

        let parsed = load(file.path()).unwrap();
        let summary = summarize(&parsed);
        assert_eq!(summary.files, 1);
        assert_eq!(summary.segments, 2);
        assert_eq!(summary.total_bytes, 1000);
    }

    #[test]
    fn test_download_decryptor_mode_selection() {
        // Unencrypted metadata yields None even if password is set (archive
        // password only); encrypted metadata without a password errors
        // fail-closed; encrypted metadata with a password creates one. A
        // whitespace-only password is a valid (if unusual) secret; only the
        // empty string is rejected.
        let meta_unencrypted = pesto::nzb::NzbMeta {
            yenc_encrypted: false,
            password: Some(fixture_kdf_input().to_string()),
            ..Default::default()
        };
        assert!(download_decryptor(&meta_unencrypted).unwrap().is_none());

        let meta_missing_pwd = pesto::nzb::NzbMeta {
            yenc_encrypted: true,
            password: None,
            ..Default::default()
        };
        let err = match download_decryptor(&meta_missing_pwd) {
            Err(e) => e,
            Ok(_) => panic!("expected error for missing password on encrypted NZB"),
        };
        assert!(err.to_string().contains("missing password metadata"));

        let meta_encrypted = pesto::nzb::NzbMeta {
            yenc_encrypted: true,
            password: Some(fixture_kdf_input().to_string()),
            ..Default::default()
        };
        assert!(download_decryptor(&meta_encrypted).unwrap().is_some());

        let meta_empty_pwd = pesto::nzb::NzbMeta {
            yenc_encrypted: true,
            password: Some(String::new()),
            ..Default::default()
        };
        assert!(download_decryptor(&meta_empty_pwd).is_err());
        let meta_whitespace_pwd = pesto::nzb::NzbMeta {
            yenc_encrypted: true,
            password: Some("   ".into()),
            ..Default::default()
        };
        assert!(download_decryptor(&meta_whitespace_pwd).is_ok());
    }

    #[test]
    fn test_download_decryptor_with_password_override() {
        // Q1(a): the CLI `--password` override is transport-only behind the
        // marker. Without the marker it never yields a decryptor; with the
        // marker it substitutes for absent/wrong meta passwords and an empty
        // override fails closed instead of falling back.
        let meta_unencrypted = pesto::nzb::NzbMeta {
            yenc_encrypted: false,
            password: Some(fixture_kdf_input().to_string()),
            ..Default::default()
        };
        assert!(
            download_decryptor_with_password_override(&meta_unencrypted, Some(fixture_kdf_input()))
                .unwrap()
                .is_none(),
            "override must not enable transport decryption without the marker"
        );

        let meta_missing_pwd = pesto::nzb::NzbMeta {
            yenc_encrypted: true,
            password: None,
            ..Default::default()
        };
        assert!(
            download_decryptor_with_password_override(&meta_missing_pwd, Some(fixture_kdf_input()))
                .unwrap()
                .is_some(),
            "explicit override must supply an absent meta password"
        );

        let meta_encrypted = pesto::nzb::NzbMeta {
            yenc_encrypted: true,
            password: Some("conformance-fixture-meta-password".into()),
            ..Default::default()
        };
        assert!(download_decryptor_with_password_override(
            &meta_encrypted,
            Some(fixture_kdf_input())
        )
        .unwrap()
        .is_some());
        assert!(
            download_decryptor_with_password_override(&meta_encrypted, None)
                .unwrap()
                .is_some()
        );
        assert!(
            download_decryptor_with_password_override(&meta_encrypted, Some("")).is_err(),
            "an empty explicit override must fail closed, not fall back to meta"
        );
    }
}
