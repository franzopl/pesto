//! `mode=addfile` (multipart upload) against the router directly via
//! `tower::ServiceExt::oneshot`.

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

fn multipart_body(filename: &str, content: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----pennewebtestboundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"name\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(content);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[tokio::test]
async fn addfile_stages_the_upload_and_it_shows_up_in_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state = support::build_state(dir.path().join("data"), config);
    let app = sugo::build_router(state);

    let (content_type, body) = multipart_body("release.nzb", &minimal_nzb_bytes());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api?mode=addfile&apikey=secret")
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], true);
    assert_eq!(json["nzo_ids"].as_array().unwrap().len(), 1);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api?mode=queue&apikey=secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["queue"]["noofslots"], 1);
    assert_eq!(json["queue"]["slots"][0]["filename"], "release");
}

#[tokio::test]
async fn addfile_without_a_file_field_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state = support::build_state(dir.path().join("data"), config);
    let app = sugo::build_router(state);

    let boundary = "----emptyboundary";
    let body = format!("--{boundary}--\r\n").into_bytes();
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api?mode=addfile&apikey=secret")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], false);
}
