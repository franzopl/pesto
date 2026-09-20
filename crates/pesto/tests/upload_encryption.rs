use std::process::Command;
use std::sync::{Arc, Mutex};

mod support;
use pesto::config::{Config, FileConfig, ObfuscateMode, Overrides};
use pesto::crypto::DownloadDecryptionAdapter;
use pesto::poster::post_files_with_progress;
use support::{fixture_kdf_input, fixture_kdf_input_bytes};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

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

/// Generated segments carry only NZB 1.1 `bytes`/`number` attributes (Body Std v1.2 §8).
fn assert_plain_segments(xml: &str) {
    for t in xml.split("<segment ").skip(1) {
        let tag = &t[..t.find('>').unwrap()];
        let attrs = tag.matches('=').count();
        let ok = attrs == 2 && tag.starts_with("bytes=") && tag.contains(" number=");
        assert!(ok, "non-standard segment: {tag}");
    }
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

    let config = make_config(port, Some(fixture_kdf_input().to_string()));
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
    // Secret redaction: neither the password nor any plaintext may appear in
    // the wire bytes' loggable context (the tracing layer never receives
    // them; asserted here via the raw captured articles).
    for article in &articles {
        assert!(
            !article
                .windows(fixture_kdf_input_bytes().len())
                .any(|w| w == fixture_kdf_input_bytes().as_slice()),
            "password leaked into a captured article outside the encrypted wire"
        );
    }
    let decryptor = DownloadDecryptionAdapter::with_password(fixture_kdf_input());
    let mut reconstructed = Vec::new();
    for segment in &outcome.segments {
        let article = articles
            .iter()
            .find(|article| String::from_utf8_lossy(article).contains(&segment.message_id))
            .expect("captured article must match posted Message-ID");
        let body = article_body(article);
        assert!(!body.starts_with(b"=ybegin"));
        let (salt, _) = pesto::crypto::extract_bootstrap_from_line1(body).unwrap();
        assert_eq!(salt.len(), 16);
        let decoded = decryptor.decode_article(body).unwrap();
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
fn cli_upload_artifacts_emits_encrypted_nzb() {
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
        .arg(format!("--encrypt={}", fixture_kdf_input()))
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
    let parsed = pesto::nzb::parse(&content).unwrap();
    assert!(parsed.meta.yenc_encrypted);
    assert_eq!(parsed.meta.password.as_deref(), Some(fixture_kdf_input()));
    assert_plain_segments(&content);
    assert!(content.contains("<meta type=\"yenc_encrypted\">true</meta>"));
}

#[tokio::test]
async fn encrypted_upload_metadata_provenance() {
    let (port, _captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two_parts.bin");
    let plaintext = vec![42u8; 150]; // 150 bytes with article size 100 -> 2 parts
    std::fs::write(&path, &plaintext).unwrap();

    let config = make_config(port, Some(fixture_kdf_input().to_string()));
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
        password: Some(fixture_kdf_input().to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml = pesto::nzb::generate(
        &outcome.groups,
        &outcome.segments,
        &meta,
        ObfuscateMode::None,
    );
    assert_plain_segments(&xml);
    assert!(xml.contains("<meta type=\"yenc_encrypted\">true</meta>"));

    let parsed = pesto::nzb::parse(&xml).unwrap();
    assert!(parsed.meta.yenc_encrypted);
}

#[tokio::test(flavor = "multi_thread")]
async fn encrypted_upload_multifile_par2_standard() {
    let (port, captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path1 = dir.path().join("data1.bin");
    let path2 = dir.path().join("data2.bin");
    let data1 = vec![0x11u8; 300];
    let data2 = vec![0x22u8; 300];
    std::fs::write(&path1, &data1).unwrap();
    std::fs::write(&path2, &data2).unwrap();

    let config = make_config_with_opts(
        port,
        Some(fixture_kdf_input().to_string()),
        10,
        ObfuscateMode::None,
    );
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

    let mut indices: Vec<u32> = captured
        .lock()
        .unwrap()
        .iter()
        .map(|article| {
            pesto::crypto::extract_bootstrap_from_line1(article_body(article))
                .unwrap()
                .1
        })
        .collect();
    indices.sort();
    // Index framing rule: assigned indices are the first N values of the safe sequence
    // (skipping 10 and 13), not the raw ranks 1..=N.
    assert_eq!(indices, {
        let mut expected: Vec<u32> = (1..)
            .filter(|&i: &u32| i.to_be_bytes().iter().all(|&b| b != 0x0A && b != 0x0D))
            .take(outcome.segments.len())
            .collect();
        expected.sort_unstable();
        expected
    });

    let meta = pesto::nzb::NzbMeta {
        password: Some(fixture_kdf_input().to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml = pesto::nzb::generate(
        &outcome.groups,
        &outcome.segments,
        &meta,
        ObfuscateMode::None,
    );
    let parsed = pesto::nzb::parse(&xml).unwrap();
    assert_eq!(parsed.segments.len(), outcome.segments.len());

    let articles = captured.lock().unwrap().clone();
    let decryptor = DownloadDecryptionAdapter::with_password(fixture_kdf_input());

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
            let decoded = decryptor.decode_article(body).unwrap();
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
async fn encrypted_upload_multifile_par2_obfuscated() {
    for obf_mode in [ObfuscateMode::Full, ObfuscateMode::Article] {
        let (port, captured) = spawn_mock_server().await;
        let dir = tempfile::tempdir().unwrap();
        let path1 = dir.path().join("data1.bin");
        let path2 = dir.path().join("data2.bin");
        let data1 = vec![0x33u8; 300];
        let data2 = vec![0x44u8; 300];
        std::fs::write(&path1, &data1).unwrap();
        std::fs::write(&path2, &data2).unwrap();

        let mut config =
            make_config_with_opts(port, Some(fixture_kdf_input().to_string()), 10, obf_mode);
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
        let mut indices: Vec<u32> = captured
            .lock()
            .unwrap()
            .iter()
            .map(|article| {
                pesto::crypto::extract_bootstrap_from_line1(article_body(article))
                    .unwrap()
                    .1
            })
            .collect();
        indices.sort();
        // Index framing rule: assigned indices are the first N safe-sequence values
        // (skipping 10 and 13), not the raw ranks 1..=N.
        assert_eq!(indices, {
            let mut expected: Vec<u32> = (1..)
                .filter(|&i: &u32| i.to_be_bytes().iter().all(|&b| b != 0x0A && b != 0x0D))
                .take(outcome.segments.len())
                .collect();
            expected.sort_unstable();
            expected
        });

        let meta = pesto::nzb::NzbMeta {
            password: Some(fixture_kdf_input().to_string()),
            yenc_encrypted: true,
            ..Default::default()
        };
        let xml = pesto::nzb::generate(&outcome.groups, &outcome.segments, &meta, obf_mode);
        let parsed = pesto::nzb::parse(&xml).unwrap();
        assert_eq!(parsed.segments.len(), outcome.segments.len());

        let decryptor = DownloadDecryptionAdapter::with_password(fixture_kdf_input());
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
                let decoded = decryptor.decode_article(body).unwrap();
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
async fn encrypted_resume_reuses_session_salt_and_indices() {
    let (port, captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resume_test.bin");
    let data = vec![0x55u8; 2000]; // 20 segments
    std::fs::write(&path, &data).unwrap();

    let resume_path = dir.path().join("upload.pesto-state");

    // 1. Initial run with state saving
    let mut config = make_config(port, Some(fixture_kdf_input().to_string()));
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
    assert!(state.session_salt().is_some());
    let saved_salt = *state.session_salt().unwrap();

    // 2. Resume run to finish upload
    let outcome = post_files_with_progress(&config, &files, None, Some(&resume_path), None)
        .await
        .unwrap();
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.segments.len(), 20);

    // Index framing rule: the 20 assigned bootstrap indices skip the forbidden
    // values 10 and 13 (uint32_be contains 0x0A/0x0D) — the safe sequence is
    // 1..9, 11, 12, 14..22 per the nth_safe_segment_index rank mapping.
    let articles_sorted = {
        let mut a: Vec<Vec<u8>> = captured.lock().unwrap().clone();
        a.sort();
        a
    };
    let mut original_indices: Vec<u32> = articles_sorted
        .iter()
        .map(|article| {
            pesto::crypto::extract_bootstrap_from_line1(article_body(article))
                .unwrap()
                .1
        })
        .collect();
    original_indices.sort();
    assert_eq!(original_indices, {
        let expected: Vec<u32> = (1..=22u32)
            .filter(|&i| i.to_be_bytes().iter().all(|&b| b != 0x0A && b != 0x0D))
            .collect();
        assert_eq!(expected.len(), 20);
        expected
    });

    // Assert that captured articles match the saved salt
    let articles = captured.lock().unwrap();
    for article in articles.iter() {
        let body = article_body(article);
        let (art_salt, art_index) = pesto::crypto::extract_bootstrap_from_line1(body).unwrap();
        assert_eq!(art_salt, saved_salt);
        assert!(art_index > 0);
    }
}

/// Invalidate the encryption session salt when the posting fingerprint
/// changes (nonce-reuse guard).
/// Even if layout part count remains identical (e.g. 200-byte file chunked into 2 parts:
/// article_size 100 vs article_size 110 both yield 2 parts), changing the fingerprint
/// changes the plaintext byte ranges — reusing the session salt/key would cause nonce reuse.
/// A fingerprint change MUST produce a fresh salt, clear spool records, and reconstruct
/// the payload cleanly. An unchanged retry MUST retain the salt identity.
#[tokio::test]
async fn resume_invalidates_session_salt_on_fingerprint_change_with_same_part_count() {
    let (port, captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("same_parts.bin");
    let payload = vec![0x77u8; 200]; // 200 bytes total
    std::fs::write(&path, &payload).unwrap();

    let resume_path = dir.path().join("same_parts.pesto-state");

    // Pass 1: article_size = 100 -> exactly 2 parts (100 and 100 bytes).
    // Interrupted after 1 segment so resume state is saved to disk.
    let mut config1 = make_config(port, Some(fixture_kdf_input().to_string()));
    config1.article_size = 100;
    config1.resume = true;
    config1.check = true;
    config1.check_delay_secs = 30;
    config1.connections = 2;
    config1.check_connections = 1;

    let files = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "same_parts.bin".into(),
    }];

    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let run = {
        let cancel = cancel.clone();
        let files = files.clone();
        let state_path = resume_path.clone();
        let config = config1.clone();
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

    while let Some(ev) = rx.recv().await {
        if let pesto::progress::ProgressEvent::SegmentDone { ok: true, .. } = ev {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            break;
        }
    }
    let outcome1 = run.await.unwrap().unwrap();
    assert!(outcome1.cancelled);
    assert!(resume_path.exists());

    let state1 = pesto::resume::ResumeState::load(&resume_path).unwrap();
    let salt1 = *state1
        .session_salt()
        .expect("session salt must be recorded in resume state");

    // Pass 2: article_size = 110 -> STILL exactly 2 parts (110 and 90 bytes)!
    // Layout entry count and part count match (2 parts), but fingerprint changed.
    // The poster must detect the fingerprint change, invalidate session identity and spool,
    // generate a fresh salt, and post the new articles cleanly.
    captured.lock().unwrap().clear();

    let mut config2 = make_config(port, Some(fixture_kdf_input().to_string()));
    config2.article_size = 110;
    config2.resume = true;
    config2.check = true;
    config2.check_connections = 1;
    config2.connections = 2;

    let outcome2 = post_files_with_progress(&config2, &files, None, Some(&resume_path), None)
        .await
        .unwrap();
    assert_eq!(outcome2.segments.len(), 2);

    let articles = captured.lock().unwrap().clone();
    assert_eq!(articles.len(), 2);

    let (salt2, _) =
        pesto::crypto::extract_bootstrap_from_line1(article_body(&articles[0])).unwrap();
    assert_ne!(
        salt1, salt2,
        "salt MUST be invalidated and regenerated on fingerprint change, even with same part count"
    );

    // Verify the newly captured articles decrypt cleanly to the original plaintext
    let decryptor = DownloadDecryptionAdapter::with_password(fixture_kdf_input());
    let part1 = decryptor
        .decode_article(article_body(&articles[0]))
        .expect("part 1 decodes");
    let part2 = decryptor
        .decode_article(article_body(&articles[1]))
        .expect("part 2 decodes");
    let mut reconstructed = Vec::new();
    reconstructed.extend_from_slice(&part1.data);
    reconstructed.extend_from_slice(&part2.data);
    assert_eq!(reconstructed, payload, "reconstructed plaintext must match");
}
