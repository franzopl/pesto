//! NZB encryption-provenance tests: writer emission, fail-closed reader
//! gating for `yenc_version`/`yenc_cipher`, mixed releases, and the
//! conformance-vector runners.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::ObfuscateMode;
use crate::nzb::{generate, parse, NzbMeta};
use crate::poster::PostedSegment;

/// Generated segments carry only NZB 1.1 `bytes`/`number` attributes (Body Std v1.2 §8).
fn assert_plain_segments(xml: &str) {
    for t in xml.split("<segment ").skip(1) {
        let tag = &t[..t.find('>').unwrap()];
        let attrs = tag.matches('=').count();
        let ok = attrs == 2 && tag.starts_with("bytes=") && tag.contains(" number=");
        assert!(ok, "non-standard segment: {tag}");
    }
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

fn make_segment(file_name: &str, part: u32, total: u32, msg_id: &str, bytes: u64) -> PostedSegment {
    PostedSegment {
        file_name: file_name.to_string(),
        file_path: Arc::from(Path::new(file_name)),
        subject_name: Arc::from(file_name),
        wire_name: Arc::from(""),
        wire_yenc_name: Arc::from(""),
        file_size: bytes * total as u64,
        part,
        total,
        message_id: format!("<{msg_id}>"),
        bytes,
        from: Arc::from("poster@example.com"),
        date: (None, Some(1774300000)),
        full_crc32: 0,
        server_idx: 0,
        file_index: 0,
        total_files: 0,
        segment_index: 0,
        plaintext_digest: None,
    }
}

/// Encrypted-meta emission and re-parse: the writer emits `yenc_encrypted`
/// (plus the optional version/cipher provenance metas) and only standard
/// NZB 1.1 `bytes`/`number` segment attributes — under full obfuscation too
/// — and the reader round-trips the provenance flag (Body Std v1.2 §8).
#[test]
fn encrypted_nzb_writer_emits_provenance_and_plain_segments() {
    let s1 = make_segment("f1.bin", 1, 1, "id1@x", 100);
    let s2 = make_segment("f2.bin", 1, 1, "id2@x", 200);

    let meta = NzbMeta {
        name: Some("test-release".into()),
        password: Some(fixture_kdf_input().to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };
    for obfuscate in [ObfuscateMode::None, ObfuscateMode::Full] {
        let xml = generate(
            &["alt.binaries.test".into()],
            &[s1.clone(), s2.clone()],
            &meta,
            obfuscate,
        );

        assert_plain_segments(&xml);
        assert!(
            xml.contains("<meta type=\"yenc_encrypted\">true</meta>"),
            "XML must contain yenc_encrypted meta element, got:\n{xml}"
        );
        assert!(xml.contains(&format!(
            "<meta type=\"password\">{}</meta>",
            fixture_kdf_input()
        )));
        assert!(xml.contains(&format!(
            "<meta type=\"yenc_version\">{}</meta>",
            super::YENC_SPEC_VERSION
        )));
        assert!(xml.contains("<meta type=\"yenc_cipher\">XChaCha20-Poly1305</meta>"));

        let parsed = parse(&xml).unwrap();
        assert!(parsed.meta.yenc_encrypted);
        assert_eq!(parsed.segments.len(), 2);
    }
}

/// Fail-closed provenance gating (Body Std v1.2 §8): a declared-encrypted
/// NZB with an unsupported `yenc_version` or `yenc_cipher` is a structural
/// parse failure, never a silent fallback to ordinary yEnc. Table-driven:
/// each case is (description, xml, expected error substring) where `None`
/// means the NZB must parse with the provenance flag set.
fn nzb_with_head_tags(head_tags: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    {head_tags}
  </head>
  <file poster="poster@example.com" date="1774300000" subject="[1/1] - &quot;file.bin&quot; yEnc (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">art1@example.com</segment>
    </segments>
  </file>
</nzb>"#
    )
}

#[test]
fn encrypted_nzb_in_nzb_1_1_dtd_namespace_parses() {
    // GAP13-03: an encrypted NZB declaring the NZB 1.1 DTD
    // (`DTD/nzb/nzb-1.1.dtd`, as emitted by the writer) instead of the
    // legacy `DTD/2003/nzb` namespace must still parse: the encryption
    // provenance meta and password are honoured and the segment is kept.
    let xml = nzb_with_head_tags(&format!(
        "<meta type=\"yenc_encrypted\">true</meta>\n    <meta type=\"password\">{}</meta>",
        fixture_kdf_input()
    ))
    .replace("DTD/2003/nzb", "DTD/nzb/nzb-1.1.dtd");
    assert!(xml.contains("DTD/nzb/nzb-1.1.dtd"), "{xml}");
    let parsed = parse(&xml).expect("NZB 1.1 DTD namespace NZB must parse");
    assert!(parsed.meta.yenc_encrypted);
    assert_eq!(parsed.meta.password.as_deref(), Some(fixture_kdf_input()));
    assert_eq!(parsed.segments.len(), 1);
}

#[test]
fn encrypted_nzb_version_and_cipher_metadata_gating() {
    let encrypted = r#"<meta type="yenc_encrypted">true</meta>"#;
    let ok_cipher = r#"<meta type="yenc_cipher">XChaCha20-Poly1305</meta>"#;
    let ok_version = format!(
        r#"<meta type="yenc_version">{}</meta>"#,
        super::YENC_SPEC_VERSION
    );

    // (head tags, expected error substring; None = must parse as encrypted)
    let cases: Vec<(String, Option<&str>)> = vec![
        // Supported current version/cipher parse and mark the NZB.
        (
            format!("{encrypted}\n    {ok_version}\n    {ok_cipher}"),
            None,
        ),
        // Matching is trimmed and ASCII case-insensitive (password never trimmed).
        (
            format!(
                "{encrypted}\n    <meta type=\"yenc_version\"> 1.2 </meta>\n    \
                 <meta type=\"yenc_cipher\">xchacha20-poly1305</meta>"
            ),
            None,
        ),
        // Any unsupported version fails closed.
        (
            format!("{encrypted}\n    <meta type=\"yenc_version\">1.3</meta>\n    {ok_cipher}"),
            Some("unsupported yenc_version"),
        ),
        (
            format!("{encrypted}\n    <meta type=\"yenc_version\">2.0</meta>\n    {ok_cipher}"),
            Some("unsupported yenc_version"),
        ),
        // Any unsupported cipher fails closed.
        (
            format!("{encrypted}\n    {ok_version}\n    <meta type=\"yenc_cipher\">AES</meta>"),
            Some("unsupported yenc_cipher"),
        ),
        // Self-closing/empty provenance values are present, empty values and
        // fail closed exactly like `<meta></meta>`.
        (
            format!("{encrypted}\n    <meta type=\"yenc_version\"/>"),
            Some("unsupported yenc_version"),
        ),
        (
            format!("{encrypted}\n    {ok_version}\n    <meta type=\"yenc_cipher\"/>"),
            Some("unsupported yenc_cipher"),
        ),
        (
            format!("{encrypted}\n    <meta type=\"yenc_version\"></meta>"),
            Some("unsupported yenc_version"),
        ),
        // Duplicates: every occurrence is validated, in any order.
        (
            format!(
                "{encrypted}\n    <meta type=\"yenc_version\">1.3</meta>\n    \
                 <meta type=\"yenc_version\">1.2</meta>"
            ),
            Some("unsupported yenc_version"),
        ),
        (
            format!(
                "{encrypted}\n    {ok_version}\n    {ok_cipher}\n    \
                 <meta type=\"yenc_cipher\">AES</meta>"
            ),
            Some("unsupported yenc_cipher"),
        ),
        (
            format!(
                "{encrypted}\n    {ok_version}\n    <meta type=\"yenc_cipher\">AES</meta>\n    \
                 {ok_cipher}"
            ),
            Some("unsupported yenc_cipher"),
        ),
        // Unencrypted NZBs keep today's behaviour: provenance metadata is
        // not validated without the `yenc_encrypted` marker (parsed without
        // the flag, so the expected error is None and the flag stays off).
        (r#"<meta type="yenc_version">1.3</meta>"#.to_string(), None),
        (r#"<meta type="yenc_version"/>"#.to_string(), None),
    ];

    for (head_tags, expected) in cases {
        let xml = nzb_with_head_tags(&head_tags);
        match expected {
            Some(err) => {
                let got = parse(&xml).unwrap_err();
                assert!(
                    got.to_string().contains(err),
                    "case {head_tags:?}: expected {err:?}, got: {got}"
                );
            }
            None => {
                let parsed =
                    parse(&xml).unwrap_or_else(|e| panic!("case {head_tags:?} must parse: {e}"));
                let marked = head_tags.contains(encrypted);
                assert_eq!(
                    parsed.meta.yenc_encrypted, marked,
                    "case {head_tags:?}: provenance flag mismatch"
                );
            }
        }
    }
}

#[test]
fn unterminated_password_meta_does_not_swallow_following_metas() {
    // PU12-02: a multi-line password meta with no `</meta>` before the next
    // tag must leave the following metas visible to the fail-closed
    // validation, and the password stays unset.
    let unterminated_password = "<meta type=\"password\">unterminated";
    let cases: Vec<(String, Option<&str>, bool)> = vec![
        // (head tags, expected error, password must be unset)
        (
            format!(
                "{unterminated_password}\n    <meta type=\"yenc_encrypted\">true</meta>\n    \
                 <meta type=\"yenc_version\">1.3</meta>"
            ),
            Some("unsupported yenc_version"),
            true,
        ),
        (
            format!(
                "{unterminated_password}\n    <meta type=\"yenc_encrypted\">true</meta>\n    \
                 <meta type=\"yenc_cipher\">AES</meta>"
            ),
            Some("unsupported yenc_cipher"),
            true,
        ),
        // Following metas are collected normally when they are valid.
        (
            format!("{unterminated_password}\n    <meta type=\"yenc_encrypted\">true</meta>"),
            None,
            true,
        ),
        // Stop at </head> as well: no password, no marker.
        (unterminated_password.to_string(), None, true),
    ];
    for (head_tags, expected, password_unset) in cases {
        let xml = nzb_with_head_tags(&head_tags);
        match expected {
            Some(err) => {
                let got = parse(&xml).unwrap_err();
                assert!(
                    got.to_string().contains(err),
                    "case {head_tags:?}: expected {err:?}, got: {got}"
                );
            }
            None => {
                let parsed =
                    parse(&xml).unwrap_or_else(|e| panic!("case {head_tags:?} must parse: {e}"));
                if password_unset {
                    assert_eq!(
                        parsed.meta.password, None,
                        "case {head_tags:?}: unterminated password must stay unset"
                    );
                }
                let marked = head_tags.contains("yenc_encrypted");
                assert_eq!(parsed.meta.yenc_encrypted, marked, "case {head_tags:?}");
            }
        }
    }
    // Stop at <file : the file/segments after an unterminated password meta
    // (with no </head> in between) are still parsed.
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">unterminated
  <file poster="poster@example.com" date="1774300000" subject="[1/1] - &quot;file.bin&quot; yEnc (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">art1@example.com</segment>
    </segments>
  </file>
</nzb>"#;
    let parsed = parse(xml).expect("file after unterminated password meta must parse");
    assert_eq!(parsed.meta.password, None);
    assert_eq!(parsed.segments.len(), 1);
}

/// A `<meta type="password">` without the `yenc_encrypted` provenance marker
/// is an archive-extraction password: the NZB is not encrypted, nothing
/// infers transport identity from it, and regenerating from the parsed
/// content emits no encryption metadata (Body Std v1.2 §8).
#[test]
fn archive_password_only_nzb_is_not_encrypted() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">conformance-fixture-archive-password</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="arbitrary subject 1">
    <groups>
      <group>alt.binaries.test</group>
    </groups>
    <segments>
      <segment bytes="750000" number="1">art-42@example.com</segment>
    </segments>
  </file>
  <file poster="poster@example.com" date="1774300000" subject="[1/1] - &quot;archive.rar&quot; yEnc (1/1)">
    <groups>
      <group>alt.binaries.test</group>
    </groups>
    <segments>
      <segment bytes="750000" number="1">art-rar-1@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    let parsed = parse(xml).expect("archive password NZB must parse cleanly");
    assert_eq!(
        parsed.meta.password.as_deref(),
        Some("conformance-fixture-archive-password")
    );
    assert!(!parsed.meta.yenc_encrypted);
    assert_eq!(parsed.segments.len(), 2);

    let regen_xml = generate(
        &["alt.binaries.test".to_string()],
        &parsed.segments,
        &parsed.meta,
        ObfuscateMode::None,
    );
    assert_plain_segments(&regen_xml);
    assert!(!regen_xml.contains("yenc_encrypted"));

    let parsed2 = parse(&regen_xml).unwrap();
    assert_eq!(parsed2.segments.len(), parsed.segments.len());
    assert!(!parsed2.meta.yenc_encrypted);
}

/// Bootstrap-only identity: an NZB may mix encrypted and ordinary files
/// under one provenance flag — the XML parses cleanly and per-segment
/// validation happens post-fetch, on the article bytes themselves.
#[test]
fn mixed_encrypted_and_ordinary_files_parse_without_identity() {
    let xml_owned = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{}</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="encrypted.bin">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">encrypted@example.com</segment>
    </segments>
  </file>
  <file poster="poster@example.com" date="1774300000" subject="ordinary.bin">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="100" number="1">ordinary@example.com</segment>
    </segments>
  </file>
</nzb>"#,
        fixture_kdf_input()
    );
    let xml = xml_owned.as_str();

    let parsed = parse(xml).expect("mixed release parses under bootstrap-only identity");
    assert!(parsed.meta.yenc_encrypted);
    assert_eq!(parsed.segments.len(), 2);
}

/// VEC-07 (index_allocation.json): uploader-skip vectors exercised against
/// the real allocator — the candidate's rank maps to the expected assigned
/// safe index (not just a predicate), which skips forward past forbidden
/// 0x0A/0x0D bytes.
#[test]
fn index_allocation_vectors_exercise_the_allocator() {
    let path = test_vectors_dir().join("index_allocation.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: serde_json::Value = serde_json::from_str(&content).unwrap();
    let vectors = fixture["vectors"].as_array().expect("vectors array");

    for vec in vectors {
        let id = vec["id"].as_str().unwrap();
        assert_eq!(vec["category"].as_str().unwrap(), "index_allocation");
        let candidate = vec["candidate_index"].as_u64().unwrap() as u32;
        let assigned = vec["expected_assigned_index"].as_u64().unwrap() as u32;
        let expected_index_bytes: Vec<u8> = vec["expected_index_bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b.as_u64().unwrap() as u8)
            .collect();
        let expected_error = vec["expected_error"].as_null();
        assert!(expected_error.is_some(), "vector {id} must be a skip case");

        // Candidate is forbidden (uint32_be contains 0x0A/0x0D).
        assert!(
            candidate
                .to_be_bytes()
                .iter()
                .any(|&b| b == 0x0A || b == 0x0D),
            "vector {id}: candidate {candidate} bytes must contain 0x0A/0x0D"
        );
        let rank = crate::poster::safe_index_rank(candidate);
        assert_eq!(
            crate::poster::nth_safe_segment_index(rank),
            Some(assigned),
            "vector {id}: allocator must assign {assigned} at rank {rank}"
        );
        assert!(assigned > candidate, "vector {id}: must skip forward");
        assert_eq!(
            assigned.to_be_bytes().to_vec(),
            expected_index_bytes,
            "vector {id}: index bytes mismatch"
        );
    }
}
