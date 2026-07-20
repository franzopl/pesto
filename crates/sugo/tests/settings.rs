//! Settings mutations (`crates/sugo/src/web/settings.rs`) against the
//! router directly via `tower::ServiceExt::oneshot`, same pattern as
//! `tests/api_queue.rs`/`api_addfile.rs`. Each test checks both the
//! in-memory `state.config` and, where relevant, that the change actually
//! made it to disk (`WebConfig::to_toml()` round trip).

mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn post_form(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn add_edit_and_delete_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state =
        support::build_state_with_config_path(dir.path().join("data"), config, config_path.clone());
    let app = sugo::build_router(state.clone());

    let resp = app
        .clone()
        .oneshot(post_form(
            "/settings/servers",
            "host=news.example.com&port=563&ssl=true&username=u&password=p&connections=8",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    {
        let cfg = state.config.read().await;
        assert_eq!(
            cfg.core.servers.len(),
            2,
            "the fixture server plus the newly added one"
        );
        let added = &cfg.core.servers[1];
        assert_eq!(added.host, "news.example.com");
        assert_eq!(added.password.as_deref(), Some("p"));
    }
    let on_disk = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        on_disk.contains("news.example.com"),
        "on-disk config: {on_disk}"
    );

    // Editing with a blank password must keep the existing credential.
    let resp = app
        .clone()
        .oneshot(post_form(
            "/settings/servers/1",
            "host=news2.example.com&port=119&username=u2&password=&connections=10",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    {
        let cfg = state.config.read().await;
        let updated = &cfg.core.servers[1];
        assert_eq!(updated.host, "news2.example.com");
        assert_eq!(
            updated.password.as_deref(),
            Some("p"),
            "blank password field should keep the old one"
        );
        assert!(
            !updated.ssl,
            "ssl checkbox omitted from the edit form means unchecked"
        );
    }

    let resp = app
        .oneshot(post_form("/settings/servers/1/delete", ""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cfg = state.config.read().await;
    assert_eq!(
        cfg.core.servers.len(),
        1,
        "only the original fixture server remains"
    );
}

#[tokio::test]
async fn update_general_persists_download_dir_retries_connections_and_mode() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state = support::build_state_with_config_path(dir.path().join("data"), config, config_path);
    let app = sugo::build_router(state.clone());

    let resp = app
        .oneshot(post_form(
            "/settings/general",
            "download_dir=%2Ftmp%2Fsugo-out&retries=5&connections=20&mode=repair",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let cfg = state.config.read().await;
    assert_eq!(
        cfg.core.download_dir,
        Some(std::path::PathBuf::from("/tmp/sugo-out"))
    );
    assert_eq!(cfg.core.retries, Some(5));
    assert_eq!(cfg.core.connections, Some(20));
    assert_eq!(cfg.core.mode, Some(penne::config::ProcessingMode::Repair));
}

#[tokio::test]
async fn add_and_delete_a_category() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state =
        support::build_state_with_config_path(dir.path().join("data"), config, config_path.clone());
    let app = sugo::build_router(state.clone());

    let resp = app
        .clone()
        .oneshot(post_form(
            "/settings/categories",
            "name=movies&dir=%2Fdownloads%2Fmovies",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    {
        let cfg = state.config.read().await;
        assert_eq!(cfg.web.categories.len(), 1);
        assert_eq!(cfg.web.categories[0].name, "movies");
        assert_eq!(
            cfg.web.categories[0].dir.as_deref(),
            Some("/downloads/movies")
        );
    }
    let on_disk = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        on_disk.contains("[[web.categories]]"),
        "on-disk config: {on_disk}"
    );

    let resp = app
        .oneshot(post_form("/settings/categories/0/delete", ""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cfg = state.config.read().await;
    assert!(cfg.web.categories.is_empty());
}

#[tokio::test]
async fn regenerating_the_api_key_invalidates_the_old_one() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "").unwrap();
    let config = support::test_web_config(&dir.path().join("downloads"), 0, "secret");
    let state = support::build_state_with_config_path(dir.path().join("data"), config, config_path);
    let app = sugo::build_router(state.clone());

    let resp = app
        .clone()
        .oneshot(post_form("/settings/apikey", ""))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);

    let new_key = { state.config.read().await.api_key().unwrap().to_string() };
    assert_ne!(new_key, "secret");

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api?mode=version&apikey=secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["status"], false, "the old api key must no longer work");
}
