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
    known.insert("art1@test", encoded.body);
    let addr = support::spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");
    let config = support::test_web_config(&download_dir, addr.port(), "secret");
    let state = support::build_state(dir.path().join("data"), config);
    sugo::job::worker::spawn(state.clone());

    let groups = vec!["alt.binaries.test".to_string()];
    let segments = vec![PostedSegment {
        file_name: "greeting.txt".into(),
        file_path: "greeting.txt".into(),
        subject_name: "greeting.txt".into(),
        file_size: article_len.max(data.len() as u64),
        part: 1,
        total: 1,
        message_id: "<art1@test>".into(),
        bytes: article_len.max(data.len() as u64),
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
    }];
    let nzb_bytes = pesto::nzb::generate(&groups, &segments, &NzbMeta::default()).into_bytes();

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
