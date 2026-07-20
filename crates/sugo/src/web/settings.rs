use askama::Template;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use serde::Deserialize;

use crate::state::SharedState;

use super::render;

pub struct ServerRow {
    pub host: String,
    pub port: String,
    pub ssl: bool,
    pub connections: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    servers: Vec<ServerRow>,
}

pub async fn page(State(state): State<SharedState>) -> Response {
    let config = state.config.read().await;
    let servers = config
        .core
        .servers
        .iter()
        .map(|s| ServerRow {
            host: s.host.clone(),
            port: s
                .port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "default".to_string()),
            ssl: s.ssl,
            connections: s
                .connections
                .map(|c| c.to_string())
                .unwrap_or_else(|| "default".to_string()),
        })
        .collect();
    drop(config);
    render(SettingsTemplate { servers })
}

/// Form fields arrive as strings even for numeric ones (an empty `<input>`
/// can't deserialize into `Option<u16>` via `serde_html_form` — it's a
/// present-but-empty string, not an absent key) — parsed by hand below
/// instead of fighting that with a stricter target type.
#[derive(Deserialize)]
pub struct AddServerForm {
    host: String,
    port: Option<String>,
    ssl: Option<String>,
    username: Option<String>,
    password: Option<String>,
    connections: Option<String>,
}

pub async fn add_server(
    State(state): State<SharedState>,
    Form(form): Form<AddServerForm>,
) -> Response {
    let entry = penne::config::RawServer {
        host: form.host,
        port: form
            .port
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse().ok()),
        ssl: form.ssl.is_some(),
        username: form.username.filter(|s| !s.is_empty()),
        password: form.password.filter(|s| !s.is_empty()),
        connections: form
            .connections
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse().ok()),
        retry_delay: None,
        name: None,
        explicit_only: false,
        group: None,
    };

    let mut config = state.config.write().await;
    config.core.servers.push(entry);
    let toml_text = match config.to_toml() {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize config: {e}"),
            )
                .into_response()
        }
    };

    if let Some(path) = &state.config_path {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if let Err(e) = tokio::fs::write(path, toml_text).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to save config: {e}"),
            )
                .into_response();
        }
    }
    drop(config);

    Redirect::to("/settings").into_response()
}
