//! SABnzbd-compatible `/api` endpoint: a single route dispatching on the
//! `mode` query parameter, matching the real SABnzbd wire shape closely
//! enough for `*arr` autoconfigure (Sonarr/Radarr/Prowlarr) to work against
//! it unmodified. See the plan's Design Decision 4 for the exact `mode`
//! subset implemented here and what's deliberately deferred.

mod addfile;
mod addurl;
mod fullstatus;
mod get_config;
mod history;
mod queue;
mod version;

use std::collections::HashMap;

use axum::extract::{Multipart, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::state::SharedState;

pub async fn get_handler(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(resp) = authorize(&state, &params).await {
        return resp;
    }
    match params.get("mode").map(String::as_str).unwrap_or_default() {
        "version" => version::handle(),
        "queue" => queue::handle(&state, &params).await,
        "history" => history::handle(&state, &params).await,
        "fullstatus" => fullstatus::handle(&state).await,
        "get_config" => get_config::handle(),
        "addurl" => addurl::handle(&state, &params).await,
        other => error_response(&format!("mode '{other}' not supported")),
    }
}

pub async fn post_handler(
    State(state): State<SharedState>,
    Query(params): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> Response {
    if let Err(resp) = authorize(&state, &params).await {
        return resp;
    }
    match params.get("mode").map(String::as_str).unwrap_or_default() {
        "addfile" => addfile::handle(&state, &params, multipart).await,
        other => error_response(&format!("mode '{other}' not supported over POST")),
    }
}

/// Every call must carry `?apikey=...` matching the configured key. No key
/// configured at all means "reject everything", never "open access" — an
/// admin who hasn't set one up yet shouldn't get an unauthenticated
/// downloader by accident.
async fn authorize(state: &SharedState, params: &HashMap<String, String>) -> Result<(), Response> {
    let configured = { state.config.read().await.api_key().map(str::to_owned) };
    let provided = params.get("apikey").cloned().unwrap_or_default();
    match configured {
        Some(key) if !key.is_empty() && constant_time_eq(&key, &provided) => Ok(()),
        _ => Err(unauthorized_response()),
    }
}

/// Hand-rolled constant-time compare (no `subtle` dependency for two short
/// strings) — an API key check should not leak how many leading bytes
/// matched via response timing.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn unauthorized_response() -> Response {
    Json(json!({"status": false, "error": "API Key Incorrect"})).into_response()
}

pub(crate) fn ok_response(value: Value) -> Response {
    Json(value).into_response()
}

pub(crate) fn error_response(message: &str) -> Response {
    Json(json!({"status": false, "error": message})).into_response()
}
