use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::ObfuscateMode;
use crate::crypto::DownloadDecryptionAdapter;
use crate::nzb::{generate, parse, parse_encrypted, NzbMeta};
use crate::poster::{PostedSegment, SegmentIdentity};

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn test_vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-vectors")
}

fn make_segment(
    file_name: &str,
    part: u32,
    total: u32,
    msg_id: &str,
    bytes: u64,
    identity: Option<SegmentIdentity>,
) -> PostedSegment {
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
        file_index: identity.map(|i| i.file_ordinal).unwrap_or(0),
        total_files: identity.map(|i| i.total_files).unwrap_or(0),
        segment_identity: identity,
    }
}

#[test]
fn test_tracer_explicit_segment_index_round_trip() {
    let identity = SegmentIdentity::explicit(1, 1, 1, 1).expect("valid explicit identity");
    let segment = make_segment(
        "testfile.bin",
        1,
        1,
        "test-msg-01@example.com",
        750000,
        Some(identity),
    );

    let meta = NzbMeta {
        name: Some("test-release".to_string()),
        password: Some("secret123".to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };

    let groups = vec!["alt.binaries.test".to_string()];
    let xml =
        generate(&groups, &[segment], &meta, ObfuscateMode::None).expect("generation must succeed");

    assert!(
        xml.contains("segmentIndex=\"1\""),
        "XML must contain explicit segmentIndex attribute, got:\n{xml}"
    );
    assert!(
        xml.contains("<meta type=\"yenc_encrypted\">true</meta>"),
        "XML must contain yenc_encrypted meta element, got:\n{xml}"
    );

    let parsed = parse(&xml).expect("parse must succeed");
    assert!(
        parsed.meta.yenc_encrypted,
        "parsed meta must be marked encrypted"
    );
    let identities = parsed
        .segment_identities
        .expect("segment_identities map must be populated");

    let parsed_id = identities
        .get("<test-msg-01@example.com>")
        .expect("segment identity must be present for message ID");
    assert_eq!(parsed_id.segment_index, 1);
    assert_eq!(parsed_id.part_number, 1);
    assert_eq!(parsed_id.file_ordinal, 0);
    assert_eq!(parsed_id.total_files, 0);

    assert_eq!(parsed.segments.len(), 1);
    let parsed_seg_id = parsed.segments[0]
        .segment_identity
        .expect("parsed segment must carry segment identity");
    assert_eq!(parsed_seg_id.segment_index, 1);
    assert_eq!(parsed_seg_id.file_ordinal, 0);
    assert_eq!(parsed_seg_id.total_files, 0);
}

#[test]
fn test_conformance_vectors_nzb_segment_identity() {
    let path = test_vectors_dir().join("nzb_segment_identity.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: serde_json::Value = serde_json::from_str(&content).unwrap();
    let vectors = fixture["vectors"].as_array().expect("vectors array");

    assert_eq!(vectors.len(), 33, "expected exactly 33 test vectors");
    let mut count = 0;

    for vec in vectors {
        let id = vec["id"].as_str().unwrap();
        let category = vec["category"].as_str().unwrap();
        let nzb_xml = vec["nzb_xml"].as_str().unwrap();

        match category {
            "valid_identity" => {
                let parsed = parse_encrypted(nzb_xml)
                    .unwrap_or_else(|e| panic!("vector {id} failed valid parsing: {e}"));
                assert!(parsed.meta.yenc_encrypted, "vector {id} must be encrypted");
                let identities = parsed
                    .segment_identities
                    .as_ref()
                    .unwrap_or_else(|| panic!("vector {id} missing identities map"));

                let expected_segs = vec["expected_segments"].as_array().unwrap();
                assert_eq!(
                    parsed.segments.len(),
                    expected_segs.len(),
                    "vector {id} segment count mismatch"
                );

                for exp in expected_segs {
                    let msg_id = exp["message_id"].as_str().unwrap();
                    let canonical_mid = if msg_id.starts_with('<') {
                        msg_id.to_string()
                    } else {
                        format!("<{msg_id}>")
                    };
                    let exp_index = exp["segment_index"].as_u64().unwrap() as u32;

                    let id_val = identities.get(&canonical_mid).unwrap_or_else(|| {
                        panic!("vector {id} missing identity for {canonical_mid}")
                    });
                    assert_eq!(
                        id_val.segment_index, exp_index,
                        "vector {id} index mismatch for {canonical_mid}"
                    );
                    assert_eq!(
                        id_val.file_ordinal, 0,
                        "vector {id} file_ordinal must be 0 (uncounted)"
                    );
                    assert_eq!(
                        id_val.total_files, 0,
                        "vector {id} total_files must be 0 (uncounted)"
                    );
                }
            }
            "invalid_identity" => {
                let expected_error = vec["expected_error"].as_str().unwrap();
                let res = parse_encrypted(nzb_xml);
                assert!(
                    res.is_err(),
                    "vector {id} expected error {expected_error}, but passed successfully"
                );
                let err_msg = res.unwrap_err().to_string();
                assert!(
                    err_msg.contains(expected_error),
                    "vector {id} error message '{err_msg}' should contain '{expected_error}'"
                );
            }
            "unencrypted_compatibility" => {
                let parsed = parse(nzb_xml)
                    .unwrap_or_else(|e| panic!("vector {id} failed unencrypted parse: {e}"));
                assert!(
                    !parsed.meta.yenc_encrypted,
                    "vector {id} must not be marked encrypted"
                );
                assert!(
                    parsed.segment_identities.is_none(),
                    "vector {id} segment_identities must be None"
                );
                for seg in &parsed.segments {
                    assert!(
                        seg.segment_identity.is_none(),
                        "vector {id} segment must have None identity"
                    );
                }
            }
            "index_tampering" => {
                let parsed = parse_encrypted(nzb_xml)
                    .unwrap_or_else(|e| panic!("vector {id} failed parse_encrypted: {e}"));
                assert!(parsed.meta.yenc_encrypted);

                let pwd = vec["transport_kdf_input"].as_str().unwrap();
                let salt_vec = hex_decode(vec["salt_hex"].as_str().unwrap());
                let mut salt = [0u8; 16];
                salt.copy_from_slice(&salt_vec);

                let adapter = DownloadDecryptionAdapter::with_password(pwd);

                if let Some(tampered_segs) = vec.get("tampered_segments").and_then(|v| v.as_array())
                {
                    for t_seg in tampered_segs {
                        let t_index = t_seg["tampered_segment_index"].as_u64().unwrap() as u32;
                        let ct = hex_decode(t_seg["ciphertext_hex"].as_str().unwrap());
                        let tag_vec = hex_decode(t_seg["tag_hex"].as_str().unwrap());
                        let mut tag = [0u8; 16];
                        tag.copy_from_slice(&tag_vec);

                        let res = adapter.decrypt_raw(salt, t_index, &ct, &tag);
                        assert!(
                            res.is_err(),
                            "vector {id} tampered segment {t_index} succeeded but must fail"
                        );
                    }
                } else {
                    let t_index = vec["tampered_segment_index"].as_u64().unwrap() as u32;
                    let ct = hex_decode(vec["ciphertext_hex"].as_str().unwrap());
                    let tag_vec = hex_decode(vec["tag_hex"].as_str().unwrap());
                    let mut tag = [0u8; 16];
                    tag.copy_from_slice(&tag_vec);

                    let res = adapter.decrypt_raw(salt, t_index, &ct, &tag);
                    assert!(
                        res.is_err(),
                        "vector {id} tampered index {t_index} succeeded but must fail"
                    );
                }
            }
            other => panic!("unknown category {other}"),
        }
        count += 1;
    }

    assert_eq!(count, 33, "must process exactly 33 vectors");
}

#[test]
fn test_reader_parses_archive_password_nzb_without_segment_identity() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">rarpassword</meta>
  </head>
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
    assert_eq!(parsed.meta.password.as_deref(), Some("rarpassword"));
    assert!(!parsed.meta.yenc_encrypted);
    assert!(parsed.segment_identities.is_none());
    assert!(parsed.segments[0].segment_identity.is_none());

    let res = parse_encrypted(xml);
    assert!(res.is_err());
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("MISSING_SEGMENT_INDEX"));
}

#[test]
fn test_nzb_writer_emits_segment_indices() {
    let id1 = SegmentIdentity::explicit(1, 2, 1, 1).unwrap();
    let id2 = SegmentIdentity::explicit(2, 2, 1, 2).unwrap();

    let s1 = make_segment("f1.bin", 1, 1, "id1@x", 100, Some(id1));
    let s2 = make_segment("f2.bin", 1, 1, "id2@x", 200, Some(id2));

    let meta = NzbMeta {
        password: Some("p1".to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml = generate(
        &["alt.binaries.test".into()],
        &[s1, s2],
        &meta,
        ObfuscateMode::None,
    )
    .unwrap();

    assert!(xml.contains("segmentIndex=\"1\""));
    assert!(xml.contains("segmentIndex=\"2\""));
    assert!(xml.contains("<meta type=\"yenc_encrypted\">true</meta>"));
    assert!(xml.contains("<meta type=\"yenc_version\">1.0</meta>"));
    assert!(xml.contains("<meta type=\"yenc_cipher\">XChaCha20-Poly1305</meta>"));

    let parsed = parse_encrypted(&xml).unwrap();
    assert_eq!(parsed.segments.len(), 2);
    assert_eq!(parsed.meta.yenc_version.as_deref(), Some("1.0"));
    assert_eq!(
        parsed.meta.yenc_cipher.as_deref(),
        Some("XChaCha20-Poly1305")
    );
    let id_map = parsed.segment_identities.unwrap();
    assert_eq!(id_map.get("<id1@x>").unwrap().segment_index, 1);
    assert_eq!(id_map.get("<id2@x>").unwrap().segment_index, 2);
}

#[test]
fn test_reader_rejects_unsupported_yenc_version_and_cipher() {
    let base_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">fixture-value</meta>
    <meta type="yenc_encrypted">true</meta>
    VERSION_TAG
    CIPHER_TAG
  </head>
  <file poster="poster@example.com" date="1774300000" subject="[1/1] - &quot;file.bin&quot; yEnc (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1" segmentIndex="1">art1@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    // Bad version
    let bad_ver_xml = base_xml
        .replace("VERSION_TAG", "<meta type=\"yenc_version\">2.0</meta>")
        .replace(
            "CIPHER_TAG",
            "<meta type=\"yenc_cipher\">XChaCha20-Poly1305</meta>",
        );
    let err = parse_encrypted(&bad_ver_xml).unwrap_err();
    assert!(err.to_string().contains("unsupported yenc_version"));

    // Bad cipher
    let bad_cipher_xml = base_xml
        .replace("VERSION_TAG", "<meta type=\"yenc_version\">1.0</meta>")
        .replace("CIPHER_TAG", "<meta type=\"yenc_cipher\">AES-GCM</meta>");
    let err2 = parse_encrypted(&bad_cipher_xml).unwrap_err();
    assert!(err2.to_string().contains("unsupported yenc_cipher"));

    // Valid version and cipher
    let valid_xml = base_xml
        .replace("VERSION_TAG", "<meta type=\"yenc_version\">1.0</meta>")
        .replace(
            "CIPHER_TAG",
            "<meta type=\"yenc_cipher\">XChaCha20-Poly1305</meta>",
        );
    let parsed = parse_encrypted(&valid_xml).unwrap();
    assert_eq!(parsed.meta.yenc_version.as_deref(), Some("1.0"));
    assert_eq!(
        parsed.meta.yenc_cipher.as_deref(),
        Some("XChaCha20-Poly1305")
    );
}

#[test]
fn test_unencrypted_nzb_compatibility() {
    let s1 = make_segment("f1.bin", 1, 1, "id1@x", 100, None);
    let s2 = make_segment("f2.bin", 1, 1, "id2@x", 200, None);

    // Case 1: unencrypted without password
    let meta_plain = NzbMeta {
        password: None,
        yenc_encrypted: false,
        ..Default::default()
    };
    let xml_plain = generate(
        &["alt.binaries.test".into()],
        &[s1.clone(), s2.clone()],
        &meta_plain,
        ObfuscateMode::None,
    )
    .unwrap();
    assert!(!xml_plain.contains("segmentIndex"));
    assert!(!xml_plain.contains("yenc_encrypted"));

    let parsed_plain = parse(&xml_plain).unwrap();
    assert!(!parsed_plain.meta.yenc_encrypted);
    assert!(parsed_plain.segment_identities.is_none());

    // Case 2: unencrypted with archive password (rar/7z)
    let meta_rar = NzbMeta {
        password: Some("archive-extract-pass".into()),
        yenc_encrypted: false,
        ..Default::default()
    };
    let xml_rar = generate(
        &["alt.binaries.test".into()],
        &[s1, s2],
        &meta_rar,
        ObfuscateMode::None,
    )
    .unwrap();
    assert!(!xml_rar.contains("segmentIndex"));
    assert!(!xml_rar.contains("yenc_encrypted"));
    assert!(xml_rar.contains("<meta type=\"password\">archive-extract-pass</meta>"));

    let parsed_rar = parse(&xml_rar).unwrap();
    assert!(!parsed_rar.meta.yenc_encrypted);
    assert_eq!(
        parsed_rar.meta.password.as_deref(),
        Some("archive-extract-pass")
    );
    assert!(parsed_rar.segment_identities.is_none());
}

#[test]
fn test_validate_segments_allows_decoupled_obfuscation_and_subsets() {
    // Uncounted geometry (0, 0)
    let id1 = SegmentIdentity::explicit(0, 0, 1, 10).unwrap();
    let id2 = SegmentIdentity::explicit(0, 0, 1, 20).unwrap();

    let s1 = make_segment("obf1.bin", 1, 1, "id1@x", 100, Some(id1));
    let s2 = make_segment("obf2.bin", 1, 1, "id2@x", 200, Some(id2));

    let meta = NzbMeta {
        password: Some("p1".to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };

    let xml = generate(
        &["alt.binaries.test".into()],
        &[s1, s2],
        &meta,
        ObfuscateMode::Full,
    )
    .unwrap();
    assert!(xml.contains("segmentIndex=\"10\""));
    assert!(xml.contains("segmentIndex=\"20\""));

    let parsed = parse_encrypted(&xml).unwrap();
    let id_map = parsed.segment_identities.unwrap();
    assert_eq!(id_map.get("<id1@x>").unwrap().segment_index, 10);
    assert_eq!(id_map.get("<id2@x>").unwrap().segment_index, 20);
}

#[test]
fn test_imported_uncounted_identity_round_trip() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">mypassword</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="arbitrary subject 1">
    <groups>
      <group>alt.binaries.test</group>
    </groups>
    <segments>
      <segment bytes="750000" number="1" segmentIndex="42">art-42@example.com</segment>
    </segments>
  </file>
  <file poster="poster@example.com" date="1774300000" subject="arbitrary subject 2">
    <groups>
      <group>alt.binaries.test</group>
    </groups>
    <segments>
      <segment bytes="750000" number="1" segmentIndex="99">art-99@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    let parsed = parse_encrypted(xml).unwrap();
    assert!(parsed.meta.yenc_encrypted);

    let groups = vec!["alt.binaries.test".to_string()];
    let regen_xml = generate(&groups, &parsed.segments, &parsed.meta, ObfuscateMode::None).unwrap();

    assert!(regen_xml.contains("segmentIndex=\"42\""));
    assert!(regen_xml.contains("segmentIndex=\"99\""));
    assert!(regen_xml.contains("<meta type=\"yenc_encrypted\">true</meta>"));

    let parsed2 = parse_encrypted(&regen_xml).unwrap();
    let id_map = parsed2.segment_identities.unwrap();
    assert_eq!(
        id_map.get("<art-42@example.com>").unwrap().segment_index,
        42
    );
    assert_eq!(
        id_map.get("<art-99@example.com>").unwrap().segment_index,
        99
    );
}
