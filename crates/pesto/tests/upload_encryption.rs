use std::process::Command;
use std::sync::{Arc, Mutex};

use pesto::config::{Config, FileConfig, ObfuscateMode, Overrides};
use pesto::crypto::{control, DownloadDecryptionAdapter};
use pesto::poster::post_files_with_progress;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::instrument::WithSubscriber;

async fn handle_connection(stream: TcpStream, captured: Arc<Mutex<Vec<Vec<u8>>>>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    write_half
        .write_all(b"200 pesto mock ready\r\n")
        .await
        .unwrap();

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap() == 0 {
            return;
        }
        let command = line.trim_end();
        if command == "POST" {
            write_half.write_all(b"340 send article\r\n").await.unwrap();
            let mut article = Vec::new();
            loop {
                let mut raw = Vec::new();
                if reader.read_until(b'\n', &mut raw).await.unwrap() == 0 {
                    return;
                }
                if raw == b".\r\n" {
                    break;
                }
                if raw.starts_with(b"..") {
                    raw.remove(0);
                }
                article.extend_from_slice(&raw);
            }
            captured.lock().unwrap().push(article);
            write_half
                .write_all(b"240 article received\r\n")
                .await
                .unwrap();
        } else if command.starts_with("STAT") {
            write_half
                .write_all(b"223 0 <id> article exists\r\n")
                .await
                .unwrap();
        } else if command.starts_with("MODE READER") {
            write_half.write_all(b"200 reader mode\r\n").await.unwrap();
        } else if command == "QUIT" {
            write_half.write_all(b"205 bye\r\n").await.unwrap();
            return;
        } else {
            write_half
                .write_all(b"500 unknown command\r\n")
                .await
                .unwrap();
        }
    }
}

async fn spawn_mock_server() -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let server_captured = Arc::clone(&captured);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(handle_connection(stream, Arc::clone(&server_captured)));
        }
    });
    (port, captured)
}

fn make_config(port: u16, encrypt_password: Option<String>) -> Config {
    let mut file = FileConfig::default();
    file.server.host = Some("127.0.0.1".into());
    file.server.port = Some(port);
    file.server.ssl = Some(false);
    file.server.connections = Some(1);
    file.posting.groups = Some(vec!["alt.test".into()]);
    file.posting.article_size = Some(100);
    file.posting.check = Some(false);
    let mut config = Config::resolve(
        file,
        Overrides {
            dry_run: Some(false),
            par2: Some(0),
            encrypt_password,
            ..Default::default()
        },
    )
    .unwrap();
    config.history = false;
    config.no_hooks = true;
    config.obfuscate = ObfuscateMode::None;
    config
}

fn make_config_with_opts(
    port: u16,
    encrypt_password: Option<String>,
    par2: u8,
    obfuscate: ObfuscateMode,
) -> Config {
    let mut file = FileConfig::default();
    file.server.host = Some("127.0.0.1".into());
    file.server.port = Some(port);
    file.server.ssl = Some(false);
    file.server.connections = Some(1);
    file.posting.groups = Some(vec!["alt.test".into()]);
    file.posting.article_size = Some(250);
    file.posting.check = Some(false);
    let mut config = Config::resolve(
        file,
        Overrides {
            dry_run: Some(false),
            par2: Some(par2),
            encrypt_password,
            ..Default::default()
        },
    )
    .unwrap();
    config.history = false;
    config.no_hooks = true;
    config.obfuscate = obfuscate;
    config
}

fn article_body(article: &[u8]) -> &[u8] {
    let marker = b"\r\n\r\n";
    let start = article
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|position| position + marker.len())
        .expect("article headers must end with CRLF CRLF");
    &article[start..]
}

#[tokio::test]
async fn encrypted_upload_round_trips_captured_articles() {
    let (port, captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("encrypted.bin");
    let plaintext: Vec<u8> = (0..250).map(|value| value as u8).collect();
    std::fs::write(&path, &plaintext).unwrap();

    let config = make_config(port, Some("test-secret-123".into()));
    let files = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "encrypted.bin".into(),
    }];
    let outcome = post_files_with_progress(&config, &files, None, None, None)
        .await
        .unwrap();
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), 3);

    let articles = captured.lock().unwrap().clone();
    assert_eq!(articles.len(), 3);
    let decryptor = DownloadDecryptionAdapter::with_password("test-secret-123");
    let mut reconstructed = Vec::new();
    for segment in &outcome.segments {
        let article = articles
            .iter()
            .find(|article| String::from_utf8_lossy(article).contains(&segment.message_id))
            .expect("captured article must match posted Message-ID");
        let body = article_body(article);
        assert!(!body.starts_with(b"=ybegin"));
        assert_eq!(control::extract_salt_from_line1(body).unwrap().len(), 16);
        let decoded = decryptor
            .decode_article(body, segment.segment_identity.map(|id| id.segment_index))
            .unwrap();
        reconstructed.extend_from_slice(&decoded.data);
    }
    assert_eq!(reconstructed, plaintext);
}

#[tokio::test]
async fn unencrypted_upload_preserves_standard_yenc_wire_format() {
    let (port, captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.bin");
    std::fs::write(&path, b"ordinary yenc payload").unwrap();

    let config = make_config(port, None);
    let files = vec![pesto::walk::InputFile {
        path,
        name: "plain.bin".into(),
    }];
    let outcome = post_files_with_progress(&config, &files, None, None, None)
        .await
        .unwrap();
    assert!(outcome.failures.is_empty());

    let articles = captured.lock().unwrap();
    assert_eq!(articles.len(), 1);
    let body = article_body(&articles[0]);
    assert!(body.starts_with(b"=ybegin"));
    assert!(!body
        .windows(b"=yencryption".len())
        .any(|window| window == b"=yencryption"));
}

#[test]
fn test_cli_upload_artifacts_emits_encrypted_nzb() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("file.bin");
    std::fs::write(&file, b"test content for encryption").unwrap();

    let nzb_dir = tmp.path().join("nzbs");
    std::fs::create_dir(&nzb_dir).unwrap();

    let bin = env!("CARGO_BIN_EXE_pesto");
    let xdg_home = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--dry-run")
        .arg("--groups")
        .arg("alt.binaries.test")
        .arg("--encrypt=testpass")
        .arg("--nzb-dir")
        .arg(&nzb_dir)
        .arg(&file)
        .output()
        .expect("failed to run pesto");

    assert!(
        output.status.success(),
        "pesto failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let nzb_file = nzb_dir.join("file.nzb");
    assert!(nzb_file.exists());
    let content = std::fs::read_to_string(&nzb_file).unwrap();
    let parsed =
        pesto::nzb::parse_encrypted(&content).expect("generated nzb must parse as encrypted");
    assert!(parsed.meta.yenc_encrypted);
    assert_eq!(parsed.meta.password.as_deref(), Some("testpass"));
    assert!(content.contains("segmentIndex=\"1\""));
}

#[test]
fn test_cli_season_consolidation_rejects_encryption() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Show.S01");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("Show.S01E01.mkv"), b"ep1").unwrap();
    std::fs::write(dir.join("Show.S01E02.mkv"), b"ep2").unwrap();

    let bin = env!("CARGO_BIN_EXE_pesto");
    let xdg_home = tempfile::tempdir().unwrap();
    let output = Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--dry-run")
        .arg("--groups")
        .arg("alt.binaries.test")
        .arg("--each")
        .arg("--season")
        .arg("--encrypt=testpass")
        .arg(&dir)
        .output()
        .expect("failed to run pesto");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("encrypted season consolidation is not supported"),
        "stderr must contain expected rejection: {stderr}"
    );
}

#[test]
fn test_cli_merge_encrypted_nzbs_preserves_encryption() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("merge_dir");
    std::fs::create_dir(&dir).unwrap();

    let xml1 = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">sharedpass</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E01">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1" segmentIndex="1">art1@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    let xml2 = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">sharedpass</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E02">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1" segmentIndex="2">art2@example.com</segment>
    </segments>
  </file>
</nzb>"#;

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
    let parsed = pesto::nzb::parse_encrypted(&merged_content)
        .expect("merged nzb must be valid encrypted nzb");
    assert!(parsed.meta.yenc_encrypted);
    assert_eq!(parsed.meta.password.as_deref(), Some("sharedpass"));
    assert_eq!(parsed.segments.len(), 2);
    let id_map = parsed.segment_identities.unwrap();
    assert_eq!(id_map.get("<art1@example.com>").unwrap().segment_index, 1);
    assert_eq!(id_map.get("<art2@example.com>").unwrap().segment_index, 2);
}

#[test]
fn test_cli_merge_encrypted_nzbs_rejects_conflicts() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("merge_dir");
    std::fs::create_dir(&dir).unwrap();

    let xml1 = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">sharedpass</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E01">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1" segmentIndex="1">art1@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    let xml2 = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="password">sharedpass</meta>
    <meta type="yenc_encrypted">true</meta>
  </head>
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E02">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1" segmentIndex="1">art2@example.com</segment>
    </segments>
  </file>
</nzb>"#;

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

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot merge encrypted NZBs with overlapping segment indices"),
        "stderr must contain expected rejection: {stderr}"
    );
}

#[test]
fn test_cli_merge_ordinary_nzbs_remains_unencrypted() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("merge_dir");
    std::fs::create_dir(&dir).unwrap();

    let xml1 = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E01">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">art1@example.com</segment>
    </segments>
  </file>
</nzb>"#;

    let xml2 = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="poster@example.com" date="1774300000" subject="Show.S01E02">
    <groups><group>alt.binaries.test</group></groups>
    <segments>
      <segment bytes="750000" number="1">art2@example.com</segment>
    </segments>
  </file>
</nzb>"#;

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
    let parsed = pesto::nzb::parse(&merged_content).expect("merged nzb must be valid ordinary nzb");
    assert!(!parsed.meta.yenc_encrypted);
    assert!(!merged_content.contains("segmentIndex"));
    assert!(!merged_content.contains("yenc_encrypted"));
}

#[tokio::test]
async fn test_encrypted_upload_emits_segment_indices() {
    let (port, _captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two_parts.bin");
    let plaintext = vec![42u8; 150]; // 150 bytes with article size 100 -> 2 parts
    std::fs::write(&path, &plaintext).unwrap();

    let config = make_config(port, Some("test-secret-123".into()));
    let files = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "two_parts.bin".into(),
    }];
    let outcome = post_files_with_progress(&config, &files, None, None, None)
        .await
        .unwrap();
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), 2);

    let meta = pesto::nzb::NzbMeta {
        name: Some("two_parts".to_string()),
        password: Some("test-secret-123".to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml = pesto::nzb::generate(
        &outcome.groups,
        &outcome.segments,
        &meta,
        ObfuscateMode::None,
    )
    .unwrap();
    assert!(xml.contains("segmentIndex=\"1\""));
    assert!(xml.contains("segmentIndex=\"2\""));
    assert!(xml.contains("<meta type=\"yenc_encrypted\">true</meta>"));

    let parsed = pesto::nzb::parse_encrypted(&xml).unwrap();
    assert!(parsed.meta.yenc_encrypted);
    let id_map = parsed.segment_identities.unwrap();
    for seg in &outcome.segments {
        let actual_id = id_map.get(&seg.message_id).unwrap();
        assert_eq!(
            actual_id.segment_index,
            seg.segment_identity.unwrap().segment_index
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_encrypted_upload_multifile_par2_standard() {
    let (port, captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path1 = dir.path().join("data1.bin");
    let path2 = dir.path().join("data2.bin");
    let data1 = vec![0x11u8; 300];
    let data2 = vec![0x22u8; 300];
    std::fs::write(&path1, &data1).unwrap();
    std::fs::write(&path2, &data2).unwrap();

    let config = make_config_with_opts(port, Some("testpass123".into()), 10, ObfuscateMode::None);
    let files = vec![
        pesto::walk::InputFile {
            path: path1.clone(),
            name: "data1.bin".into(),
        },
        pesto::walk::InputFile {
            path: path2.clone(),
            name: "data2.bin".into(),
        },
    ];
    let outcome = post_files_with_progress(&config, &files, None, None, None)
        .await
        .unwrap();
    assert!(outcome.failures.is_empty());

    // Must contain data files, standalone PAR2 index, and recovery volumes
    assert!(outcome.segments.iter().any(|s| s.file_name == "data1.bin"));
    assert!(outcome.segments.iter().any(|s| s.file_name == "data2.bin"));
    assert!(outcome
        .segments
        .iter()
        .any(|s| s.file_name.ends_with(".par2") && !s.file_name.contains(".vol")));
    assert!(outcome
        .segments
        .iter()
        .any(|s| s.file_name.contains(".vol")));

    let mut indices: Vec<u32> = outcome
        .segments
        .iter()
        .map(|s| s.segment_identity.unwrap().segment_index)
        .collect();
    indices.sort();
    assert_eq!(
        indices,
        (1..=outcome.segments.len() as u32).collect::<Vec<u32>>()
    );

    let meta = pesto::nzb::NzbMeta {
        password: Some("testpass123".into()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml = pesto::nzb::generate(
        &outcome.groups,
        &outcome.segments,
        &meta,
        ObfuscateMode::None,
    )
    .unwrap();
    let parsed = pesto::nzb::parse_encrypted(&xml).unwrap();
    assert_eq!(parsed.segments.len(), outcome.segments.len());

    let articles = captured.lock().unwrap().clone();
    let decryptor = DownloadDecryptionAdapter::with_password("testpass123");

    let mut file_names: Vec<String> = outcome
        .segments
        .iter()
        .map(|s| s.file_name.clone())
        .collect();
    file_names.sort();
    file_names.dedup();

    for fname in &file_names {
        let mut f_segs: Vec<&pesto::poster::PostedSegment> = outcome
            .segments
            .iter()
            .filter(|s| &s.file_name == fname)
            .collect();
        f_segs.sort_by_key(|s| s.part);
        let mut decrypted_file_bytes = Vec::new();
        for seg in f_segs {
            let article = articles
                .iter()
                .find(|a| String::from_utf8_lossy(a).contains(&seg.message_id))
                .unwrap();
            let body = article_body(article);
            let decoded = decryptor
                .decode_article(body, seg.segment_identity.map(|id| id.segment_index))
                .unwrap();
            decrypted_file_bytes.extend_from_slice(&decoded.data);
        }

        if fname == "data1.bin" {
            assert_eq!(decrypted_file_bytes, data1);
        } else if fname == "data2.bin" {
            assert_eq!(decrypted_file_bytes, data2);
        } else if fname.ends_with(".par2") {
            let packets = parmesan::packet_reader::read_packets(&decrypted_file_bytes);
            assert!(!packets.is_empty());
            assert!(packets
                .iter()
                .any(|p| p.packet_type == parmesan::packet::TYPE_MAIN));
            assert!(packets
                .iter()
                .any(|p| p.packet_type == parmesan::packet::TYPE_CREATOR));
            assert!(packets
                .iter()
                .any(|p| p.packet_type == parmesan::packet::TYPE_FILE_DESC));
            assert!(packets
                .iter()
                .any(|p| p.packet_type == parmesan::packet::TYPE_IFSC));
            if fname.contains(".vol") {
                assert!(packets
                    .iter()
                    .any(|p| p.packet_type == parmesan::packet::TYPE_RECOVERY));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_encrypted_upload_multifile_par2_obfuscated() {
    for obf_mode in [ObfuscateMode::Full, ObfuscateMode::Article] {
        let (port, captured) = spawn_mock_server().await;
        let dir = tempfile::tempdir().unwrap();
        let path1 = dir.path().join("data1.bin");
        let path2 = dir.path().join("data2.bin");
        let data1 = vec![0x33u8; 300];
        let data2 = vec![0x44u8; 300];
        std::fs::write(&path1, &data1).unwrap();
        std::fs::write(&path2, &data2).unwrap();

        let mut config = make_config_with_opts(port, Some("obfpass456".into()), 10, obf_mode); // ggignore
        config.file_counter = false;
        let files = vec![
            pesto::walk::InputFile {
                path: path1.clone(),
                name: "data1.bin".into(),
            },
            pesto::walk::InputFile {
                path: path2.clone(),
                name: "data2.bin".into(),
            },
        ];
        let outcome = post_files_with_progress(&config, &files, None, None, None)
            .await
            .unwrap();
        assert!(outcome.failures.is_empty());

        let articles = captured.lock().unwrap().clone();
        assert_eq!(articles.len(), outcome.segments.len());

        // Privacy inspection: NO source filenames, release names, or [N/M] counters in raw NNTP articles
        for article in &articles {
            let article_str = String::from_utf8_lossy(article);
            assert!(!article_str.contains("data1.bin"));
            assert!(!article_str.contains("data2.bin"));
            assert!(!article_str.contains("[1/"));
            assert!(!article_str.contains("[2/"));
            assert!(!article_str.contains("[3/"));
        }

        // Verify continuous segment indices
        let mut indices: Vec<u32> = outcome
            .segments
            .iter()
            .map(|s| s.segment_identity.unwrap().segment_index)
            .collect();
        indices.sort();
        assert_eq!(
            indices,
            (1..=outcome.segments.len() as u32).collect::<Vec<u32>>()
        );

        let meta = pesto::nzb::NzbMeta {
            password: Some("obfpass456".into()),
            yenc_encrypted: true,
            ..Default::default()
        };
        let xml =
            pesto::nzb::generate(&outcome.groups, &outcome.segments, &meta, obf_mode).unwrap();
        let parsed = pesto::nzb::parse_encrypted(&xml).unwrap();
        assert_eq!(parsed.segments.len(), outcome.segments.len());

        let decryptor = DownloadDecryptionAdapter::with_password("obfpass456");
        let mut file_names: Vec<String> = outcome
            .segments
            .iter()
            .map(|s| s.file_name.clone())
            .collect();
        file_names.sort();
        file_names.dedup();

        for fname in &file_names {
            let mut f_segs: Vec<&pesto::poster::PostedSegment> = outcome
                .segments
                .iter()
                .filter(|s| &s.file_name == fname)
                .collect();
            f_segs.sort_by_key(|s| s.part);
            let mut decrypted_file_bytes = Vec::new();
            for seg in &f_segs {
                let article = articles
                    .iter()
                    .find(|a| String::from_utf8_lossy(a).contains(&seg.message_id))
                    .unwrap();
                let body = article_body(article);
                let decoded = decryptor
                    .decode_article(body, seg.segment_identity.map(|id| id.segment_index))
                    .unwrap();
                decrypted_file_bytes.extend_from_slice(&decoded.data);
            }

            let on_disk = std::fs::read(&f_segs[0].file_path).unwrap();
            assert_eq!(
                decrypted_file_bytes, on_disk,
                "decrypted bytes mismatch for {fname}"
            );

            if fname == "data1.bin" {
                assert_eq!(decrypted_file_bytes, data1);
            } else if fname == "data2.bin" {
                assert_eq!(decrypted_file_bytes, data2);
            } else {
                let packets = parmesan::packet_reader::read_packets(&decrypted_file_bytes);
                assert!(packets
                    .iter()
                    .any(|p| p.packet_type == parmesan::packet::TYPE_MAIN));
                assert!(packets
                    .iter()
                    .any(|p| p.packet_type == parmesan::packet::TYPE_CREATOR));
                assert!(packets
                    .iter()
                    .any(|p| p.packet_type == parmesan::packet::TYPE_FILE_DESC));
                assert!(packets
                    .iter()
                    .any(|p| p.packet_type == parmesan::packet::TYPE_IFSC));
                assert!(packets
                    .iter()
                    .any(|p| p.packet_type == parmesan::packet::TYPE_RECOVERY));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_persistence_lifecycle_invariants() {
    let (port, captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resume_test.bin");
    let data = vec![0x55u8; 2000]; // 20 segments
    std::fs::write(&path, &data).unwrap();

    let resume_path = dir.path().join("upload.pesto-state");

    // 1. Initial run with state saving
    let mut config = make_config(port, Some("resumepass".into()));
    config.connections = 2;
    config.resume = true;
    config.check = true;
    config.check_delay_secs = 30; // Delays check phase to give time for cancel
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

    let files = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "resume_test.bin".into(),
    }];

    let run = {
        let cancel = cancel.clone();
        let files = files.clone();
        let state_path = resume_path.clone();
        let config = config.clone();
        tokio::spawn(async move {
            pesto::poster::post_files_with_progress_and_cancel(
                &config,
                &files,
                Some(tx),
                Some(&state_path),
                Some(cancel),
                None,
            )
            .await
        })
    };

    let mut done = 0usize;
    while let Some(ev) = rx.recv().await {
        if let pesto::progress::ProgressEvent::SegmentDone { ok: true, .. } = ev {
            done += 1;
            if done >= 1 {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    let outcome1 = run.await.unwrap().unwrap();
    assert!(outcome1.cancelled);
    assert!(resume_path.exists());

    // Read back saved resume state
    let state =
        pesto::resume::ResumeState::load(&resume_path).expect("resume state must be written");
    assert!(state.session_identity().is_some());
    let session_ident = state.session_identity().unwrap();
    let saved_salt = session_ident.salt().expect("salt must be set");

    // 2. Resume run to finish upload
    let outcome = post_files_with_progress(&config, &files, None, Some(&resume_path), None)
        .await
        .unwrap();
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), 20);

    let original_indices: Vec<u32> = outcome
        .segments
        .iter()
        .map(|s| s.segment_identity.unwrap().segment_index)
        .collect();
    assert_eq!(original_indices, (1..=20).collect::<Vec<u32>>());

    // 3. Simulate post-check article repost check & NZB regeneration
    let meta = pesto::nzb::NzbMeta {
        password: Some("resumepass".into()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml1 = pesto::nzb::generate(
        &outcome.groups,
        &outcome.segments,
        &meta,
        ObfuscateMode::None,
    )
    .unwrap();
    let xml2 = pesto::nzb::generate(
        &outcome.groups,
        &outcome.segments,
        &meta,
        ObfuscateMode::None,
    )
    .unwrap();
    assert_eq!(xml1, xml2);

    let parsed = pesto::nzb::parse_encrypted(&xml1).unwrap();
    let id_map = parsed.segment_identities.unwrap();
    for seg in &outcome.segments {
        assert_eq!(
            id_map.get(&seg.message_id).unwrap().segment_index,
            seg.segment_identity.unwrap().segment_index
        );
    }

    // Assert that captured articles match the saved salt
    let articles = captured.lock().unwrap();
    for article in articles.iter() {
        let body = article_body(article);
        let art_salt = control::extract_salt_from_line1(body).unwrap();
        assert_eq!(&art_salt, saved_salt);
    }
}

#[tokio::test]
async fn test_secret_redaction_in_tracing_logs() {
    let sentinel_pass = "SENTINEL_PASSWORD_998877";
    let (port, _captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log_test.bin");
    let plaintext = b"SECRET_PLAINTEXT_DATA_12345";
    std::fs::write(&path, plaintext).unwrap();

    let log_buffer = Arc::new(Mutex::new(Vec::new()));
    let buffer_clone = Arc::clone(&log_buffer);

    struct BufferWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let make_writer = move || BufferWriter(Arc::clone(&buffer_clone));

    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(make_writer)
        .finish();

    let config = make_config(port, Some(sentinel_pass.into()));
    let files = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "log_test.bin".into(),
    }];

    let outcome = post_files_with_progress(&config, &files, None, None, Some("redaction-capture"))
        .with_subscriber(subscriber)
        .await
        .unwrap();
    assert!(outcome.failures.is_empty());

    let logs = String::from_utf8_lossy(&log_buffer.lock().unwrap()).to_string();

    // Verify sentinels are completely absent from logs
    assert!(
        !logs.contains(sentinel_pass),
        "password sentinel leaked in logs"
    );
    assert!(
        !logs.contains("SECRET_PLAINTEXT_DATA_12345"),
        "plaintext leaked in logs"
    );

    // Safe identifiers may be logged
    assert!(
        logs.contains("redaction-capture") && logs.contains("upload plan"),
        "logs should contain upload-specific progress context"
    );
}
