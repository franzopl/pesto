//! Integration tests for multi-file encrypted releases with real PAR2 recovery (Phase 14).
//!
//! Validates:
//! 1. Downloading and decrypting multiple payload files and real PAR2 recovery volumes
//!    via mock-NNTP using explicit indices.
//! 2. Clean verification of intact decrypted files via PAR2 Reed-Solomon checksums.
//! 3. Bit-identical repair of a damaged decrypted payload file using the decrypted recovery volume.

mod support;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;

use penne::config::ServerTier;
use penne::download::download_queue_with_decryptor;
use penne::nzb::{download_decryptor, load};
use penne::queue::build;
use penne::repair::{verify_and_repair, RepairOutcome};
use pesto::crypto::kdf::EncryptionSession;
use pesto::crypto::UploadEncryptionAdapter;
use pesto::poster::SegmentIdentity;
use pesto::yenc::PartSpec;
use tempfile::NamedTempFile;

use support::{build_fixture_set, server_entry, spawn_mock_nntp_server, FixtureFile};

fn write_temp_nzb(xml: &str) -> NamedTempFile {
    let mut temp = NamedTempFile::new().expect("failed to create temp file");
    temp.write_all(xml.as_bytes()).expect("failed to write XML");
    temp
}

fn encode_article(
    session: &Arc<EncryptionSession>,
    file_name: &str,
    file_size: u64,
    spec: PartSpec,
    payload: &[u8],
    segment_index: u32,
) -> Vec<u8> {
    let uploader = UploadEncryptionAdapter::new(session.clone());
    let identity = SegmentIdentity::explicit(0, 0, spec.number, segment_index).unwrap();
    let mut body = Vec::new();
    let encoded = uploader
        .encode_article(
            file_name, file_size, spec, payload, 128, None, identity, &mut body,
        )
        .expect("encode article failed");
    encoded.body
}

struct EncryptedPar2Release {
    xml: String,
    known_articles: HashMap<String, Vec<u8>>,
    data1: Vec<u8>,
    data2: Vec<u8>,
}

fn build_encrypted_par2_fixture(password: &str) -> EncryptedPar2Release {
    let data1: Vec<u8> = (0..1024u32).map(|i| (i * 7) as u8).collect();
    let data2: Vec<u8> = (0..1536u32).map(|i| (i * 13) as u8).collect();

    let fixture_dir = build_fixture_set(
        &[
            FixtureFile {
                name: "data1.bin",
                data: data1.clone(),
            },
            FixtureFile {
                name: "data2.bin",
                data: data2.clone(),
            },
        ],
        256,
        8,
    );

    let base_par2_bytes = std::fs::read(fixture_dir.join("base.par2")).unwrap();
    let vol_filename = "base.vol000+008.par2";
    let vol_par2_bytes = std::fs::read(fixture_dir.join(vol_filename)).unwrap();
    let _ = std::fs::remove_dir_all(&fixture_dir);

    let salt = [0x5au8; 16];
    let session = Arc::new(EncryptionSession::new(password, salt).unwrap());

    let mut known_articles = HashMap::new();
    let mut next_index = 1u32;

    // Encrypt data1.bin (split into 2 parts of 512 bytes)
    let d1_p1 = &data1[..512];
    let d1_p2 = &data1[512..];
    let art_d1_p1 = encode_article(
        &session,
        "data1.bin",
        data1.len() as u64,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        d1_p1,
        next_index,
    );
    let msg_d1_p1 = "msg-data1-01@test".to_string();
    known_articles.insert(msg_d1_p1.clone(), art_d1_p1);
    let idx_d1_p1 = next_index;
    next_index += 1;

    let art_d1_p2 = encode_article(
        &session,
        "data1.bin",
        data1.len() as u64,
        PartSpec {
            number: 2,
            total: 2,
            offset: 512,
        },
        d1_p2,
        next_index,
    );
    let msg_d1_p2 = "msg-data1-02@test".to_string();
    known_articles.insert(msg_d1_p2.clone(), art_d1_p2);
    let idx_d1_p2 = next_index;
    next_index += 1;

    // Encrypt data2.bin (1 part of 1536 bytes)
    let art_d2 = encode_article(
        &session,
        "data2.bin",
        data2.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &data2,
        next_index,
    );
    let msg_d2 = "msg-data2-01@test".to_string();
    known_articles.insert(msg_d2.clone(), art_d2);
    let idx_d2 = next_index;
    next_index += 1;

    // Encrypt base.par2 (1 part)
    let art_base_par2 = encode_article(
        &session,
        "base.par2",
        base_par2_bytes.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &base_par2_bytes,
        next_index,
    );
    let msg_base_par2 = "msg-base-par2@test".to_string();
    known_articles.insert(msg_base_par2.clone(), art_base_par2);
    let idx_base_par2 = next_index;
    next_index += 1;

    // Encrypt base.vol000+008.par2 (1 part)
    let art_vol_par2 = encode_article(
        &session,
        vol_filename,
        vol_par2_bytes.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &vol_par2_bytes,
        next_index,
    );
    let msg_vol_par2 = "msg-vol-par2@test".to_string();
    known_articles.insert(msg_vol_par2.clone(), art_vol_par2);
    let idx_vol_par2 = next_index;

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{password}</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;data1.bin&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="1" segmentIndex="{idx_d1_p1}">{msg_d1_p1}</segment>
      <segment bytes="{}" number="2" segmentIndex="{idx_d1_p2}">{msg_d1_p2}</segment>
    </segments>
  </file>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;data2.bin&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="1" segmentIndex="{idx_d2}">{msg_d2}</segment>
    </segments>
  </file>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;base.par2&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="1" segmentIndex="{idx_base_par2}">{msg_base_par2}</segment>
    </segments>
  </file>
  <file poster="uploader@example.com" date="1774300000" subject="&quot;{vol_filename}&quot; yEnc">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="{}" number="1" segmentIndex="{idx_vol_par2}">{msg_vol_par2}</segment>
    </segments>
  </file>
</nzb>"#,
        d1_p1.len(),
        d1_p2.len(),
        data2.len(),
        base_par2_bytes.len(),
        vol_par2_bytes.len()
    );

    EncryptedPar2Release {
        xml,
        known_articles,
        data1,
        data2,
    }
}

#[tokio::test]
async fn test_encrypted_multifile_par2_verify_ok() {
    let password = "encrypted-par2-verify-pass";
    let release = build_encrypted_par2_fixture(password);

    let addr = spawn_mock_nntp_server(release.known_articles, None);
    let nzb_file = write_temp_nzb(&release.xml);
    let parsed = load(nzb_file.path()).unwrap();
    let queue = build(&parsed);
    let decryptor = download_decryptor(&parsed.meta).unwrap();
    let dest = tempfile::tempdir().unwrap();

    let outcome = download_queue_with_decryptor(
        &queue,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
        0,
        None,
        decryptor,
    )
    .await
    .unwrap();

    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());
    assert_eq!(outcome.assembled.len(), 4);

    let known_files: HashSet<String> = [
        "data1.bin".to_string(),
        "data2.bin".to_string(),
        "base.par2".to_string(),
        "base.vol000+008.par2".to_string(),
    ]
    .into_iter()
    .collect();

    let repair_res = verify_and_repair(dest.path(), &outcome.assembled, &known_files, None)
        .await
        .unwrap();

    assert!(
        matches!(repair_res, RepairOutcome::Ok),
        "expected RepairOutcome::Ok, got: {:?}",
        repair_res
    );

    assert_eq!(
        std::fs::read(dest.path().join("data1.bin")).unwrap(),
        release.data1
    );
    assert_eq!(
        std::fs::read(dest.path().join("data2.bin")).unwrap(),
        release.data2
    );
}

#[tokio::test]
async fn test_encrypted_multifile_par2_repair_damaged_file() {
    let password = "encrypted-par2-repair-pass";
    let release = build_encrypted_par2_fixture(password);

    let addr = spawn_mock_nntp_server(release.known_articles, None);
    let nzb_file = write_temp_nzb(&release.xml);
    let parsed = load(nzb_file.path()).unwrap();
    let queue = build(&parsed);
    let decryptor = download_decryptor(&parsed.meta).unwrap();
    let dest = tempfile::tempdir().unwrap();

    let outcome = download_queue_with_decryptor(
        &queue,
        &[ServerTier::solo(server_entry(addr))],
        dest.path(),
        0,
        None,
        decryptor,
    )
    .await
    .unwrap();

    assert!(outcome.missing.is_empty());
    assert!(outcome.corrupt.is_empty());

    // Corrupt data1.bin intentionally on disk (overwrite first 256 bytes)
    let d1_path = dest.path().join("data1.bin");
    let mut corrupted = std::fs::read(&d1_path).unwrap();
    for byte in corrupted.iter_mut().take(256) {
        *byte ^= 0xaa;
    }
    std::fs::write(&d1_path, &corrupted).unwrap();

    let known_files: HashSet<String> = [
        "data1.bin".to_string(),
        "data2.bin".to_string(),
        "base.par2".to_string(),
        "base.vol000+008.par2".to_string(),
    ]
    .into_iter()
    .collect();

    // With a damaged file, assembled outcome map can either be empty or reflect the damage
    let repair_res = verify_and_repair(dest.path(), &HashMap::new(), &known_files, None)
        .await
        .unwrap();

    match repair_res {
        RepairOutcome::Repaired(plan) => {
            assert!(
                plan.repaired_files.iter().any(|f| f.name == "data1.bin"),
                "plan must report data1.bin as repaired"
            );
        }
        other => panic!("expected RepairOutcome::Repaired, got: {other:?}"),
    }

    // Verify bit-identical restoration
    let repaired_bytes = std::fs::read(&d1_path).unwrap();
    assert_eq!(
        repaired_bytes, release.data1,
        "repaired file must match original plaintext bit-identically"
    );
    assert_eq!(
        std::fs::read(dest.path().join("data2.bin")).unwrap(),
        release.data2
    );
}
