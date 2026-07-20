//! `mode=version`/`mode=queue` against the router directly via
//! `tower::ServiceExt::oneshot` — no real socket needed.

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn minimal_nzb_bytes() -> Vec<u8> {
    let groups = vec!["alt.binaries.test".to_string()];
    let segments = vec![pesto::poster::PostedSegment {
        file_name: "release.bin".into(),
        file_path: "release.bin".into(),
        subject_name: "release.bin".into(),
        file_size: 10,
        part: 1,
        total: 1,
        message_id: "<art1@test>".into(),
        bytes: 10,
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
    }];
    pesto::nzb::generate(&groups, &segments, &pesto::nzb::NzbMeta::default()).into_bytes()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn every_mode_requires_a_valid_api_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state = support::build_state(dir.path().join("data"), config);
    let app = sugo::build_router(state);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api?mode=version")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], false);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api?mode=version&apikey=wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["status"], false);
}

#[tokio::test]
async fn version_returns_ok_with_the_right_key() {
    let dir = tempfile::tempdir().unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state = support::build_state(dir.path().join("data"), config);
    let app = sugo::build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api?mode=version&apikey=secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert!(json["version"].as_str().unwrap().starts_with("sugo-"));
}

#[tokio::test]
async fn queue_reflects_a_staged_job() {
    let dir = tempfile::tempdir().unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state = support::build_state(dir.path().join("data"), config);

    let job = sugo::job::stage_and_create(&state, "release.nzb", None, minimal_nzb_bytes())
        .await
        .unwrap();
    state.jobs.write().await.enqueue(job);

    let app = sugo::build_router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api?mode=queue&apikey=secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(resp).await;
    assert_eq!(json["queue"]["noofslots"], 1);
    assert_eq!(json["queue"]["slots"][0]["status"], "Queued");
    assert_eq!(json["queue"]["slots"][0]["filename"], "release");
}
