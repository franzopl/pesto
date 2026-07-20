//! Settings page and every mutating form behind it: add/edit/delete
//! `[[servers]]`, general config (`download_dir`/`retries`/`connections`/
//! `mode`), `[[web.categories]]` CRUD, and API key rotation. Every mutating
//! handler follows the same shape: lock `state.config`, mutate, clone the
//! result, drop the lock, [`persist`] the clone to `state.config_path`,
//! redirect back to `/settings`.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use penne::config::{ProcessingMode, RawServer};
use serde::Deserialize;

use crate::config::{CategoryConfig, WebConfig};
use crate::state::SharedState;

use super::render;

pub struct ServerRow {
    pub index: usize,
    pub host: String,
    pub port: String,
    pub ssl: bool,
    pub username: String,
    pub connections: String,
}

pub struct CategoryRow {
    pub index: usize,
    pub name: String,
    pub dir: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsTemplate {
    servers: Vec<ServerRow>,
    categories: Vec<CategoryRow>,
    download_dir: String,
    retries: String,
    connections: String,
    mode: String,
    api_key: String,
}

pub async fn page(State(state): State<SharedState>) -> Response {
    let config = state.config.read().await;
    let servers = config
        .core
        .servers
        .iter()
        .enumerate()
        .map(|(index, s)| ServerRow {
            index,
            host: s.host.clone(),
            port: s.port.map(|p| p.to_string()).unwrap_or_default(),
            ssl: s.ssl,
            username: s.username.clone().unwrap_or_default(),
            connections: s.connections.map(|c| c.to_string()).unwrap_or_default(),
        })
        .collect();
    let categories = config
        .web
        .categories
        .iter()
        .enumerate()
        .map(|(index, c)| CategoryRow {
            index,
            name: c.name.clone(),
            dir: c.dir.clone().unwrap_or_default(),
        })
        .collect();
    let download_dir = config
        .core
        .download_dir
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let retries = config
        .core
        .retries
        .map(|r| r.to_string())
        .unwrap_or_default();
    let connections = config
        .core
        .connections
        .map(|c| c.to_string())
        .unwrap_or_default();
    let mode = mode_to_str(config.core.mode.unwrap_or_default()).to_string();
    let api_key = config.web.api_key.clone().unwrap_or_default();
    drop(config);
    render(SettingsTemplate {
        servers,
        categories,
        download_dir,
        retries,
        connections,
        mode,
        api_key,
    })
}

fn mode_to_str(mode: ProcessingMode) -> &'static str {
    match mode {
        ProcessingMode::Download => "download",
        ProcessingMode::Repair => "repair",
        ProcessingMode::Unpack => "unpack",
        ProcessingMode::Delete => "delete",
    }
}

fn parse_mode(s: &str) -> Option<ProcessingMode> {
    match s {
        "download" => Some(ProcessingMode::Download),
        "repair" => Some(ProcessingMode::Repair),
        "unpack" => Some(ProcessingMode::Unpack),
        "delete" => Some(ProcessingMode::Delete),
        _ => None,
    }
}

/// Serializes `config` to TOML and writes it to `state.config_path` (if
/// any was given at startup) — the shared tail every settings mutation
/// below ends with.
async fn persist(state: &SharedState, config: &WebConfig) -> Result<(), Response> {
    let toml_text = config.to_toml().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to serialize config: {e}"),
        )
            .into_response()
    })?;
    if let Some(path) = &state.config_path {
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::write(path, toml_text).await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to save config: {e}"),
            )
                .into_response()
        })?;
    }
    Ok(())
}

/// Form fields arrive as strings even for numeric ones (an empty `<input>`
/// can't deserialize into `Option<u16>` via `serde_html_form` — it's a
/// present-but-empty string, not an absent key) — parsed by hand below
/// instead of fighting that with a stricter target type.
#[derive(Deserialize)]
pub struct ServerForm {
    host: String,
    port: Option<String>,
    ssl: Option<String>,
    username: Option<String>,
    password: Option<String>,
    connections: Option<String>,
}

fn build_server(
    form: ServerForm,
    name: Option<String>,
    explicit_only: bool,
    group: Option<u32>,
) -> RawServer {
    RawServer {
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
        name,
        explicit_only,
        group,
    }
}

pub async fn add_server(
    State(state): State<SharedState>,
    Form(form): Form<ServerForm>,
) -> Response {
    let entry = build_server(form, None, false, None);
    let snapshot = {
        let mut config = state.config.write().await;
        config.core.servers.push(entry);
        config.clone()
    };
    if let Err(resp) = persist(&state, &snapshot).await {
        return resp;
    }
    Redirect::to("/settings").into_response()
}

/// Leaving the password field blank on an edit keeps the existing
/// credential instead of clearing it — re-saving a server's other fields
/// shouldn't require retyping its password every time.
pub async fn update_server(
    Path(index): Path<usize>,
    State(state): State<SharedState>,
    Form(form): Form<ServerForm>,
) -> Response {
    let snapshot = {
        let mut config = state.config.write().await;
        let Some(existing) = config.core.servers.get(index).cloned() else {
            return (StatusCode::NOT_FOUND, "no such server").into_response();
        };
        let mut updated = build_server(form, existing.name, existing.explicit_only, existing.group);
        if updated.password.is_none() {
            updated.password = existing.password;
        }
        config.core.servers[index] = updated;
        config.clone()
    };
    if let Err(resp) = persist(&state, &snapshot).await {
        return resp;
    }
    Redirect::to("/settings").into_response()
}

pub async fn delete_server(Path(index): Path<usize>, State(state): State<SharedState>) -> Response {
    let snapshot = {
        let mut config = state.config.write().await;
        if index >= config.core.servers.len() {
            return (StatusCode::NOT_FOUND, "no such server").into_response();
        }
        config.core.servers.remove(index);
        config.clone()
    };
    if let Err(resp) = persist(&state, &snapshot).await {
        return resp;
    }
    Redirect::to("/settings").into_response()
}

#[derive(Deserialize)]
pub struct GeneralForm {
    download_dir: Option<String>,
    retries: Option<String>,
    connections: Option<String>,
    mode: Option<String>,
}

pub async fn update_general(
    State(state): State<SharedState>,
    Form(form): Form<GeneralForm>,
) -> Response {
    let snapshot = {
        let mut config = state.config.write().await;
        config.core.download_dir = form
            .download_dir
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from);
        config.core.retries = form
            .retries
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse().ok());
        config.core.connections = form
            .connections
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse().ok());
        config.core.mode = form.mode.as_deref().and_then(parse_mode);
        config.clone()
    };
    if let Err(resp) = persist(&state, &snapshot).await {
        return resp;
    }
    Redirect::to("/settings").into_response()
}

#[derive(Deserialize)]
pub struct AddCategoryForm {
    name: String,
    dir: Option<String>,
}

pub async fn add_category(
    State(state): State<SharedState>,
    Form(form): Form<AddCategoryForm>,
) -> Response {
    let name = form.name.trim().to_string();
    if name.is_empty() || name == "*" {
        return (
            StatusCode::BAD_REQUEST,
            "category name must be non-empty and not \"*\" (that one always exists implicitly)",
        )
            .into_response();
    }
    let snapshot = {
        let mut config = state.config.write().await;
        config.web.categories.push(CategoryConfig {
            name,
            dir: form.dir.filter(|s| !s.is_empty()),
        });
        config.clone()
    };
    if let Err(resp) = persist(&state, &snapshot).await {
        return resp;
    }
    Redirect::to("/settings").into_response()
}

pub async fn delete_category(
    Path(index): Path<usize>,
    State(state): State<SharedState>,
) -> Response {
    let snapshot = {
        let mut config = state.config.write().await;
        if index >= config.web.categories.len() {
            return (StatusCode::NOT_FOUND, "no such category").into_response();
        }
        config.web.categories.remove(index);
        config.clone()
    };
    if let Err(resp) = persist(&state, &snapshot).await {
        return resp;
    }
    Redirect::to("/settings").into_response()
}

pub async fn regenerate_api_key(State(state): State<SharedState>) -> Response {
    let new_key = uuid::Uuid::new_v4().simple().to_string();
    let snapshot = {
        let mut config = state.config.write().await;
        config.web.api_key = Some(new_key);
        config.clone()
    };
    if let Err(resp) = persist(&state, &snapshot).await {
        return resp;
    }
    Redirect::to("/settings").into_response()
}
