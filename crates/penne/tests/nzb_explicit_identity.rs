use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;

use penne::nzb::{load, load_encrypted};
use penne::queue::build;
use tempfile::NamedTempFile;

fn test_vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-vectors")
}

fn write_temp_nzb(xml: &str) -> NamedTempFile {
    let mut temp = NamedTempFile::new().expect("failed to create temp file");
    temp.write_all(xml.as_bytes()).expect("failed to write XML");
    temp
}

#[test]
fn test_tracer_penne_queue_explicit_identity() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">tracer_secret</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="uploader@example.com" date="1774300000" subject="completely-obfuscated-subject-no-counters">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1" segmentIndex="42">tracer-msg-01@example.com</segment>
      <segment bytes="250000" number="2" segmentIndex="43">tracer-msg-02@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    let temp = write_temp_nzb(xml);
    let parsed = load(temp.path()).expect("Penne NZB load must succeed");
    assert!(parsed.meta.yenc_encrypted, "must be marked yenc_encrypted");

    // Cryptographic segment identity in reader must have uncounted (0, 0) geometry
    let identities = parsed
        .segment_identities
        .as_ref()
        .expect("segment_identities must be present");
    let id1 = identities
        .get("<tracer-msg-01@example.com>")
        .expect("identity for msg 1");
    assert_eq!(id1.segment_index, 42);
    assert_eq!(id1.file_ordinal, 0, "file_ordinal must be 0 (uncounted)");
    assert_eq!(id1.total_files, 0, "total_files must be 0 (uncounted)");

    let id2 = identities
        .get("<tracer-msg-02@example.com>")
        .expect("identity for msg 2");
    assert_eq!(id2.segment_index, 43);
    assert_eq!(id2.file_ordinal, 0, "file_ordinal must be 0 (uncounted)");
    assert_eq!(id2.total_files, 0, "total_files must be 0 (uncounted)");

    // Queue build must propagate explicit indices directly
    let queue = build(&parsed);
    assert_eq!(queue.files.len(), 1);
    let queued_file = &queue.files[0];
    assert_eq!(queued_file.segments.len(), 2);

    assert_eq!(
        queued_file.segments[0].message_id,
        "<tracer-msg-01@example.com>"
    );
    assert_eq!(queued_file.segments[0].segment_index, Some(42));

    assert_eq!(
        queued_file.segments[1].message_id,
        "<tracer-msg-02@example.com>"
    );
    assert_eq!(queued_file.segments[1].segment_index, Some(43));
}

#[test]
fn test_conformance_vectors_penne_nzb_segment_identity() {
    let path = test_vectors_dir().join("nzb_segment_identity.json");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let fixture: serde_json::Value = serde_json::from_str(&content).unwrap();
    let vectors = fixture["vectors"].as_array().expect("vectors array");

    assert_eq!(vectors.len(), 33, "expected exactly 33 test vectors");
    let mut count = 0;
    let mut seen_error_tokens = HashSet::new();

    for vec in vectors {
        let id = vec["id"].as_str().unwrap();
        let category = vec["category"].as_str().unwrap();
        let nzb_xml = vec["nzb_xml"].as_str().unwrap();

        match category {
            "valid_identity" => {
                let temp = write_temp_nzb(nzb_xml);
                let parsed =
                    load(temp.path()).unwrap_or_else(|e| panic!("vector {id} failed load: {e}"));
                assert!(parsed.meta.yenc_encrypted, "vector {id} must be encrypted");

                // Assert cryptographic identities maintain uncounted (0, 0) geometry
                if let Some(ref identities) = parsed.segment_identities {
                    for (mid, id_val) in identities {
                        assert_eq!(
                            id_val.file_ordinal, 0,
                            "vector {id} message {mid} file_ordinal must be 0 (uncounted)"
                        );
                        assert_eq!(
                            id_val.total_files, 0,
                            "vector {id} message {mid} total_files must be 0 (uncounted)"
                        );
                    }
                }

                let queue = build(&parsed);
                let actual: HashMap<&str, Option<u32>> = queue
                    .files
                    .iter()
                    .flat_map(|f| &f.segments)
                    .map(|s| (s.message_id.as_str(), s.segment_index))
                    .collect();

                let expected_segs = vec["expected_segments"].as_array().unwrap();
                assert_eq!(
                    actual.len(),
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

                    let &actual_index = actual
                        .get(canonical_mid.as_str())
                        .unwrap_or_else(|| panic!("vector {id} missing message {canonical_mid}"));
                    assert_eq!(
                        actual_index,
                        Some(exp_index),
                        "vector {id} index mismatch for {canonical_mid}"
                    );
                }
            }
            "invalid_identity" => {
                let expected_error = vec["expected_error"].as_str().unwrap();
                seen_error_tokens.insert(expected_error.to_string());

                let temp = write_temp_nzb(nzb_xml);
                let res = load_encrypted(temp.path());
                assert!(
                    res.is_err(),
                    "vector {id} expected error {expected_error}, but passed successfully"
                );
                let err_msg = format!("{:#}", res.unwrap_err());
                assert!(
                    err_msg.contains(expected_error),
                    "vector {id} error message '{err_msg}' should contain '{expected_error}'"
                );

                if id != "nzb-invalid-17-missing-index-encrypted" {
                    let standard_res = load(temp.path());
                    assert!(
                        standard_res.is_err(),
                        "vector {id} expected standard load error {expected_error}, but passed"
                    );
                    let std_err_msg = format!("{:#}", standard_res.unwrap_err());
                    assert!(
                        std_err_msg.contains(expected_error),
                        "vector {id} standard load error '{std_err_msg}' should contain '{expected_error}'"
                    );
                } else {
                    // Vector 17 lacks any segmentIndex, so standard unforced load treats it as
                    // backward-compatible archive password NZB
                    let standard_parsed =
                        load(temp.path()).expect("vector 17 standard load succeeds as unencrypted");
                    assert!(!standard_parsed.meta.yenc_encrypted);
                    assert!(standard_parsed.segment_identities.is_none());
                }
            }
            "unencrypted_compatibility" => {
                let temp = write_temp_nzb(nzb_xml);
                let parsed = load(temp.path())
                    .unwrap_or_else(|e| panic!("vector {id} failed unencrypted parse: {e}"));
                assert!(
                    !parsed.meta.yenc_encrypted,
                    "vector {id} must not be marked encrypted"
                );
                assert!(
                    parsed.segment_identities.is_none(),
                    "vector {id} segment_identities must be None"
                );

                let queue = build(&parsed);
                for file in &queue.files {
                    for seg in &file.segments {
                        assert_eq!(
                            seg.segment_index, None,
                            "vector {id} queued segment must have None segment_index"
                        );
                    }
                }
            }
            "index_tampering" => {
                let temp = write_temp_nzb(nzb_xml);
                let parsed = load(temp.path())
                    .unwrap_or_else(|e| panic!("vector {id} failed parse_encrypted: {e}"));
                assert!(
                    parsed.meta.yenc_encrypted,
                    "vector {id} must be marked encrypted"
                );

                let queue = build(&parsed);
                if let Some(tampered_segs) = vec.get("tampered_segments").and_then(|v| v.as_array())
                {
                    let actual: HashMap<&str, Option<u32>> = queue
                        .files
                        .iter()
                        .flat_map(|f| &f.segments)
                        .map(|s| (s.message_id.as_str(), s.segment_index))
                        .collect();
                    for t_seg in tampered_segs {
                        let msg_id = t_seg["message_id"].as_str().unwrap();
                        let canonical_mid = if msg_id.starts_with('<') {
                            msg_id.to_string()
                        } else {
                            format!("<{msg_id}>")
                        };
                        let t_index = t_seg["tampered_segment_index"].as_u64().unwrap() as u32;
                        assert_eq!(
                            actual.get(canonical_mid.as_str()),
                            Some(&Some(t_index)),
                            "vector {id} tampered segment index must be queued"
                        );
                    }
                } else {
                    let t_index = vec["tampered_segment_index"].as_u64().unwrap() as u32;
                    assert_eq!(
                        queue.files[0].segments[0].segment_index,
                        Some(t_index),
                        "vector {id} tampered index must be queued"
                    );
                }
            }
            other => panic!("unknown category {other}"),
        }
        count += 1;
    }

    assert_eq!(count, 33, "must process exactly 33 vectors");

    let required_tokens = [
        "INVALID_SEGMENT_INDEX_EMPTY",
        "INVALID_SEGMENT_INDEX_ZERO",
        "INVALID_SEGMENT_INDEX_SIGN",
        "INVALID_SEGMENT_INDEX_WHITESPACE",
        "INVALID_SEGMENT_INDEX_LEADING_ZERO",
        "INVALID_SEGMENT_INDEX_NON_DIGIT",
        "INVALID_SEGMENT_INDEX_OVERFLOW",
        "MISSING_SEGMENT_INDEX",
        "CONFLICTING_MESSAGE_ID_INDEX",
        "DUPLICATE_SEGMENT_INDEX",
    ];
    for token in required_tokens {
        assert!(
            seen_error_tokens.contains(token),
            "missing assertion coverage for canonical error token '{token}'"
        );
    }
}

#[test]
fn test_compatibility_archive_password_only() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">rarpassword</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="[1/1] - &quot;archive.rar&quot; yEnc (1/1)">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">archive-msg-01@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    let temp = write_temp_nzb(xml);
    let parsed = load(temp.path()).expect("load must succeed");
    assert!(
        !parsed.meta.yenc_encrypted,
        "must NOT be marked yenc_encrypted"
    );
    assert_eq!(parsed.meta.password.as_deref(), Some("rarpassword"));
    assert!(parsed.segment_identities.is_none());

    let queue = build(&parsed);
    assert_eq!(queue.files.len(), 1);
    assert_eq!(queue.files[0].segments[0].segment_index, None);
}
