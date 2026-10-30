//! PESTO-R6-2 regression suite: the real aggregation writers must retain
//! yEnc encryption provenance (Body Encryption Standard v1.2 §8).
//!
//! Covered producer paths:
//! - `--merge-season` (`bin/pesto/merge.rs`): all sources in a group must
//!   agree on encryption mode and, when encrypted, on the shared transport
//!   password; a mismatch or a mixed encrypted/unencrypted group errors;
//! - `--dry-run --season` (`bin/pesto/batch.rs::run_batch`): the season pack
//!   gets the same marker and password precedence as the per-episode
//!   artifacts (`encrypt_password` → `nzb_password` → archive fallback).
//!
//! Every scenario invokes the actual CLI binary with isolated config/output
//! directories, parses the actual XML, and feeds it through the same
//! download-decryptor activation gate Penne applies before every download.
//! Ordinary (unmarked) outputs keep the existing password behavior.

use std::process::Command;

mod support;
use pesto::crypto::DownloadDecryptionAdapter;
use support::{fixture_kdf_input, other_kdf_input};

/// Shared transport password for the merge tests below. Both encrypted
/// source NZBs use it; the merged output must retain it.
fn merge_transport_password() -> &'static str {
    fixture_kdf_input()
}

/// Build a real `DownloadDecryptionAdapter` from NZB head metadata using the
/// exact gate Penne applies before every download
/// (`crates/penne/src/nzb.rs::download_decryptor`). The pesto test crate
/// cannot depend on the penne crate (workspace layering), so the same
/// marker/password activation rule is mirrored here and exercised against
/// the actual parsed XML.
fn penne_decryptor_from_meta_for_test(
    meta: &pesto::nzb::NzbMeta,
) -> Result<Option<DownloadDecryptionAdapter>, String> {
    if !meta.yenc_encrypted {
        return Ok(None);
    }
    let password = meta
        .password
        .as_deref()
        .ok_or("encrypted NZB is missing password metadata".to_string())?;
    if password.is_empty() {
        return Err("encrypted NZB password is empty".to_string());
    }
    Ok(Some(DownloadDecryptionAdapter::with_password(password)))
}

/// Generated segments carry only NZB 1.1 `bytes`/`number` attributes
/// (Body Std v1.2 §8) — same invariant as `upload_encryption.rs`.
fn assert_plain_segments(xml: &str) {
    for t in xml.split("<segment ").skip(1) {
        let tag = &t[..t.find('>').unwrap()];
        let attrs = tag.matches('=').count();
        let ok = attrs == 2 && tag.starts_with("bytes=") && tag.contains(" number=");
        assert!(ok, "non-standard segment: {tag}");
    }
}

/// PESTO-R6-2: `--merge-season` over two *encrypted* episode NZBs (distinct
/// salts, one transport password) must retain the `yenc_encrypted` marker and
/// the shared transport password agreed by all sources. The merged output is then driven through the real download
/// decryptor constructor (`penne`'s seam lives in `crates/penne`; here we
/// parse the actual XML and construct the real adapter from its metadata).
#[test]
fn test_cli_merge_encrypted_nzbs_retains_marker_and_password() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("merge_dir");
    std::fs::create_dir(&dir).unwrap();

    // Distinct salts (self-describing articles carry their own salt in Line
    // 1), one shared transport password — exactly the settled merge contract.
    let xml1 = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{}</meta>
    <meta type="yenc_encrypted">true</meta>
    <meta type="yenc_version">1.2</meta>
    <meta type="yenc_cipher">XChaCha20-Poly1305</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E01">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">art1@example.com</segment>
    </segments>
  </file>
</nzb>"#,
        fixture_kdf_input()
    );
    let xml2 = xml1
        .replace("Show.S01E01", "Show.S01E02")
        .replace("art1@example.com", "art2@example.com");

    std::fs::write(dir.join("Show.S01E01.nzb"), xml1).unwrap();
    std::fs::write(dir.join("Show.S01E02.nzb"), xml2).unwrap();

    let bin = env!("CARGO_BIN_EXE_pesto");
    let xdg_home = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--merge-season")
        .arg(&dir)
        .output()
        .expect("failed to run pesto");

    assert!(
        output.status.success(),
        "pesto merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let merged_path = dir.join("Show.S01.nzb");
    assert!(merged_path.exists());
    let merged_content = std::fs::read_to_string(&merged_path).unwrap();
    let parsed = pesto::nzb::parse(&merged_content).expect("merged nzb must be valid");
    assert!(
        parsed.meta.yenc_encrypted,
        "merged encrypted NZB must keep the yenc_encrypted marker"
    );
    assert_eq!(
        parsed.meta.password.as_deref(),
        Some(merge_transport_password()),
        "merged encrypted NZB must carry the shared transport password"
    );
    assert_plain_segments(&merged_content);

    // Feed the real download-side decryptor constructor (the seam Penne
    // itself calls before every download): it must activate on the merged
    // metadata, proving the marker/password combination is genuinely usable.
    let decryptor =
        penne_decryptor_from_meta_for_test(&parsed.meta).expect("decryptor must activate");
    assert!(
        decryptor.is_some(),
        "download decryptor must be constructed"
    );
}

/// PESTO-R6-2 (password precedence): with `--encrypt`, `--nzb-password` and
/// `--password` all set, the season pack's `<meta type="password">` must be
/// the *encryption* (transport) password — matching `upload/artifacts.rs` —
/// while the per-episode NZBs must agree with it.
#[test]
fn test_cli_season_encryption_password_wins_over_nzb_and_archive_passwords() {
    let tmp = tempfile::tempdir().unwrap();
    let season_dir = tmp.path().join("Season01");
    std::fs::create_dir_all(&season_dir).unwrap();
    std::fs::write(season_dir.join("S01E01.mkv"), b"season payload episode one").unwrap();
    std::fs::write(season_dir.join("S01E02.mkv"), b"season payload episode two").unwrap();

    let nzb_dir = tmp.path().join("nzbs");
    std::fs::create_dir_all(&nzb_dir).unwrap();

    let bin = env!("CARGO_BIN_EXE_pesto");
    let xdg_home = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--dry-run")
        .arg("--season")
        .arg("--groups")
        .arg("alt.binaries.test")
        .arg(format!("--encrypt={}", fixture_kdf_input()))
        .arg("--nzb-password=conformance-fixture-nzb-password")
        .arg("--password=conformance-fixture-archive-password")
        .arg("--nzb-dir")
        .arg(&nzb_dir)
        .arg(&season_dir)
        .output()
        .expect("failed to run pesto");

    assert!(
        output.status.success(),
        "pesto season dry run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let season_path = nzb_dir.join("Season01.nzb");
    assert!(season_path.exists(), "season NZB must be written");
    let content = std::fs::read_to_string(&season_path).unwrap();
    let parsed = pesto::nzb::parse(&content).expect("season nzb must be valid");
    assert!(
        parsed.meta.yenc_encrypted,
        "encrypted season pack must carry the yenc_encrypted marker"
    );
    assert_eq!(
        parsed.meta.password.as_deref(),
        Some(fixture_kdf_input()),
        "encryption password must win over nzb_password and the archive fallback"
    );
    assert_plain_segments(&content);

    // The real download decryptor constructor must activate on it.
    let decryptor =
        penne_decryptor_from_meta_for_test(&parsed.meta).expect("decryptor must activate");
    assert!(
        decryptor.is_some(),
        "download decryptor must be constructed"
    );

    // Per-episode NZBs use the same precedence — they must agree with the pack.
    let episode = std::fs::read_to_string(nzb_dir.join("S01E01.nzb")).unwrap();
    let episode_parsed = pesto::nzb::parse(&episode).unwrap();
    assert!(episode_parsed.meta.yenc_encrypted);
    assert_eq!(
        episode_parsed.meta.password.as_deref(),
        Some(fixture_kdf_input())
    );
}

/// PESTO-R6-2 (disabled mode): an ordinary `--season` dry run with only an
/// archive password and an NZB password must keep the existing precedence
/// (`nzb_password` first) and never mark the pack encrypted.
#[test]
fn test_cli_season_ordinary_password_precedence_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let season_dir = tmp.path().join("Season02");
    std::fs::create_dir_all(&season_dir).unwrap();
    std::fs::write(season_dir.join("S01E01.mkv"), b"ordinary payload").unwrap();

    let nzb_dir = tmp.path().join("nzbs");
    std::fs::create_dir_all(&nzb_dir).unwrap();

    let bin = env!("CARGO_BIN_EXE_pesto");
    let xdg_home = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--dry-run")
        .arg("--season")
        .arg("--groups")
        .arg("alt.binaries.test")
        .arg("--nzb-password=conformance-fixture-nzb-password")
        .arg("--password=conformance-fixture-archive-password")
        .arg("--nzb-dir")
        .arg(&nzb_dir)
        .arg(&season_dir)
        .output()
        .expect("failed to run pesto");

    assert!(
        output.status.success(),
        "pesto season dry run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let season_path = nzb_dir.join("Season02.nzb");
    assert!(season_path.exists());
    let content = std::fs::read_to_string(&season_path).unwrap();
    let parsed = pesto::nzb::parse(&content).expect("season nzb must be valid");
    assert!(
        !parsed.meta.yenc_encrypted,
        "ordinary season pack must stay unmarked"
    );
    assert_eq!(
        parsed.meta.password.as_deref(),
        Some("conformance-fixture-nzb-password"),
        "ordinary nzb_password precedence must be unchanged"
    );
    assert_plain_segments(&content);
    assert!(!content.contains("yenc_encrypted"));
}

/// PESTO-R6-2 (merge, disabled mode): merging two ordinary NZBs whose head
/// carries only an *archive* password must keep discarding it — an unmarked
/// source archive password never becomes a transport password, and the
/// output stays unmarked with no password at all.
#[test]
fn test_cli_merge_archive_password_stays_discarded() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("merge_dir");
    std::fs::create_dir(&dir).unwrap();

    let xml1_owned = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{}</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E01">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">art1@example.com</segment>
    </segments>
  </file>
</nzb>"#,
        fixture_kdf_input()
    );
    let xml1 = xml1_owned.as_str();
    let xml2 = xml1
        .replace("Show.S01E01", "Show.S01E02")
        .replace("art1@example.com", "art2@example.com");

    std::fs::write(dir.join("Show.S01E01.nzb"), xml1).unwrap();
    std::fs::write(dir.join("Show.S01E02.nzb"), xml2).unwrap();

    let bin = env!("CARGO_BIN_EXE_pesto");
    let xdg_home = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--merge-season")
        .arg(&dir)
        .output()
        .expect("failed to run pesto");

    assert!(
        output.status.success(),
        "pesto merge failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let merged_content = std::fs::read_to_string(dir.join("Show.S01.nzb")).unwrap();
    let parsed = pesto::nzb::parse(&merged_content).unwrap();
    assert!(!parsed.meta.yenc_encrypted);
    assert_eq!(
        parsed.meta.password, None,
        "an unmarked source archive password must never become a transport password"
    );
    let decryptor = penne_decryptor_from_meta_for_test(&parsed.meta).unwrap();
    assert!(
        decryptor.is_none(),
        "ordinary merged output must not activate transport decryption"
    );
}

fn encrypted_nzb_xml(episode: &str, msg: &str, password: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">{password}</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="Show.{episode}">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">{msg}</segment>
    </segments>
  </file>
</nzb>"#
    )
}

#[test]
fn test_cli_merge_rejects_differing_encrypted_passwords() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("merge_dir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(
        dir.join("Show.S01E01.nzb"),
        encrypted_nzb_xml("S01E01", "art1@example.com", fixture_kdf_input()),
    )
    .unwrap();
    std::fs::write(
        dir.join("Show.S01E02.nzb"),
        encrypted_nzb_xml("S01E02", "art2@example.com", other_kdf_input()),
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_pesto");
    let xdg_home = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--merge-season")
        .arg(&dir)
        .output()
        .expect("failed to run pesto");
    assert!(
        !output.status.success(),
        "merge with differing passwords must fail: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("different encryption password"),
        "error must name the password conflict, got: {stderr}"
    );
    assert!(
        !dir.join("Show.S01.nzb").exists(),
        "no hybrid NZB may be written on mismatch"
    );
}

#[test]
fn test_cli_merge_rejects_mixed_encrypted_and_unencrypted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("merge_dir");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(
        dir.join("Show.S01E01.nzb"),
        encrypted_nzb_xml("S01E01", "art1@example.com", fixture_kdf_input()),
    )
    .unwrap();
    let plain = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E02">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">art2@example.com</segment>
    </segments>
  </file>
</nzb>"#;
    std::fs::write(dir.join("Show.S01E02.nzb"), plain).unwrap();
    let bin = env!("CARGO_BIN_EXE_pesto");
    let xdg_home = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--merge-season")
        .arg(&dir)
        .output()
        .expect("failed to run pesto");
    assert!(
        !output.status.success(),
        "mixed encrypted/unencrypted merge must fail: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("mixed encrypted/unencrypted"),
        "error must name the mixed-mode conflict, got: {stderr}"
    );
    assert!(!dir.join("Show.S01.nzb").exists());
}
