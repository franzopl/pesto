//! Full pipeline test: a fake NNTP server, the real background job worker,
//! and the pipeline (`crates/sugo/src/job/pipeline.rs`) — a job staged
//! into the queue is actually fetched, assembled, and lands in history,
//! mirroring `crates/penne/tests/cli_download_end_to_end.rs`'s coverage of
//! `penne download` itself but through this crate's job engine instead of
//! the CLI.

mod support;

use std::collections::HashMap;
use std::time::Duration;

use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;
use pesto::yenc::{encode_part, PartSpec};
use support::fixture_kdf_input;

#[tokio::test]
async fn a_staged_job_downloads_and_lands_in_history() {
    let data = b"hello from sugo end-to-end test".to_vec();
    let encoded = encode_part(
        "greeting.txt",
        data.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &data,
        128,
        None,
    );
    let article_len = encoded.body.len() as u64;

    let mut known = HashMap::new();
    known.insert("art1@test".to_string(), encoded.body);
    let addr = support::spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");
    let config = support::test_web_config(&download_dir, addr.port(), "secret");
    let state = support::build_state(dir.path().join("data"), config);
    sugo::job::worker::spawn(state.clone());

    let groups = vec!["alt.binaries.test".to_string()];
    let segments = vec![PostedSegment {
        file_name: "greeting.txt".into(),
        file_path: std::path::Path::new("greeting.txt").into(),
        subject_name: "greeting.txt".into(),
        wire_name: "greeting.txt".into(),
        wire_yenc_name: "greeting.txt".into(),
        file_size: article_len.max(data.len() as u64),
        part: 1,
        total: 1,
        message_id: "<art1@test>".into(),
        bytes: article_len.max(data.len() as u64),
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 0,
        total_files: 0,
        segment_index: 0,
        plaintext_digest: None,
    }];
    let nzb_bytes = pesto::nzb::generate(
        &groups,
        &segments,
        &NzbMeta::default(),
        pesto::config::ObfuscateMode::None,
    )
    .into_bytes();

    let job = sugo::job::stage_and_create(&state, "greeting.nzb", None, nzb_bytes)
        .await
        .unwrap();
    state.jobs.write().await.enqueue(job);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        {
            let store = state.jobs.read().await;
            if let Some(finished) = store.history.first() {
                assert_eq!(
                    finished.status,
                    sugo::job::JobStatus::Completed,
                    "job finished with an unexpected status: {:?} ({:?})",
                    finished.status,
                    finished.message
                );
                assert_eq!(
                    finished.files.len(),
                    1,
                    "expected one per-file progress entry"
                );
                assert!(
                    finished
                        .files
                        .iter()
                        .all(|f| f.done && f.bytes_done == f.bytes_total),
                    "every file should be marked done with bytes_done == bytes_total: {:?}",
                    finished.files
                );
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job did not reach history within the timeout"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // `stage_and_create` gives each job its own subdirectory
    // (`download_dir/<job name>/`), named after the `.nzb`'s file stem.
    let written = std::fs::read(download_dir.join("greeting").join("greeting.txt")).unwrap();
    assert_eq!(written, data);
}

/// Shared encrypted-job driver: builds an encrypted single-segment NZB,
/// stages it as a sugo job, and waits for history. `tamper` flips one byte
/// of the ciphertext data line to exercise the failure path.
async fn run_encrypted_job(
    file_name: &str,
    message_id: &str,
    tamper: bool,
) -> (sugo::job::JobStatus, Option<String>) {
    let kdf_input = fixture_kdf_input();
    let salt = pesto::crypto::generate_alphabet_salt();
    let session =
        std::sync::Arc::new(pesto::crypto::EncryptionSession::new(kdf_input, salt).unwrap());
    let adapter = pesto::crypto::UploadEncryptionAdapter::new(session);

    let original = b"Plaintext content payload for encrypted Sugo job test!";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let mut body = Vec::new();
    let encoded = adapter
        .encode_article(
            file_name,
            original.len() as u64,
            spec,
            original,
            128,
            1,
            &mut body,
        )
        .unwrap();

    let wire_body = if tamper {
        let mut lines: Vec<Vec<u8>> = encoded
            .body
            .split_inclusive(|&b| b == b'\n')
            .map(|l| l.to_vec())
            .collect();
        assert!(lines.len() >= 4);
        let data = lines.len() - 2;
        lines[data][0] ^= 0x01;
        lines.into_iter().flatten().collect()
    } else {
        encoded.body
    };

    let mut known: HashMap<String, Vec<u8>> = HashMap::new();
    known.insert(message_id.to_string(), wire_body);
    let addr = support::spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");
    let config = support::test_web_config(&download_dir, addr.port(), "secret");
    let state = support::build_state(dir.path().join("data"), config);
    sugo::job::worker::spawn(state.clone());

    let groups = vec!["alt.binaries.test".to_string()];
    let segments = vec![PostedSegment {
        file_name: file_name.into(),
        file_path: std::path::Path::new(file_name).into(),
        subject_name: file_name.into(),
        wire_name: file_name.into(),
        wire_yenc_name: file_name.into(),
        file_size: original.len() as u64,
        part: 1,
        total: 1,
        message_id: format!("<{message_id}>"),
        bytes: original.len() as u64,
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 1,
        total_files: 1,
        segment_index: 0,
        plaintext_digest: None,
    }];
    let meta = NzbMeta {
        password: Some(kdf_input.to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let nzb_bytes = pesto::nzb::generate(
        &groups,
        &segments,
        &meta,
        pesto::config::ObfuscateMode::None,
    )
    .into_bytes();

    let job = sugo::job::stage_and_create(&state, &format!("{file_name}.nzb"), None, nzb_bytes)
        .await
        .unwrap();
    state.jobs.write().await.enqueue(job);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        {
            let store = state.jobs.read().await;
            if let Some(finished) = store.history.first() {
                return (finished.status, finished.message.clone());
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "job did not reach history within the timeout"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn encrypted_nzb_sugo_job_round_trip() {
    let (status, _) = run_encrypted_job("sugo_secret.bin", "sugo_enc1@test", false).await;
    assert_eq!(
        status,
        sugo::job::JobStatus::Completed,
        "job finished with unexpected status: {status:?}"
    );
}

#[tokio::test]
async fn encrypted_nzb_sugo_tampered_job_fails_cleanly() {
    let kdf_input = fixture_kdf_input();
    let (status, message) = run_encrypted_job("tampered.bin", "sugo_tamper@test", true).await;
    assert_eq!(
        status,
        sugo::job::JobStatus::Failed,
        "expected JobStatus::Failed for tampered job, got: {status:?}"
    );
    if let Some(ref msg) = message {
        assert!(
            !msg.contains(kdf_input),
            "error message leaked password canary"
        );
    }
}
