use std::collections::HashMap;

use axum::response::Response;
use serde_json::json;

use crate::job;
use crate::state::SharedState;

use super::{error_response, ok_response};

/// `mode=addurl&name=<nzb-url>` — fetches the `.nzb` over HTTP (via
/// `reqwest`, already a workspace dependency elsewhere) rather than
/// requiring the caller to download and re-upload it via `mode=addfile`.
pub async fn handle(state: &SharedState, params: &HashMap<String, String>) -> Response {
    let Some(url) = params.get("name").filter(|s| !s.is_empty()) else {
        return error_response("missing 'name' (nzb URL)");
    };
    let category = params.get("cat").cloned();

    let response = match reqwest::get(url.as_str()).await {
        Ok(resp) => resp,
        Err(e) => return error_response(&format!("failed fetching {url}: {e}")),
    };
    let response = match response.error_for_status() {
        Ok(resp) => resp,
        Err(e) => return error_response(&format!("failed fetching {url}: {e}")),
    };
    let bytes = match response.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => return error_response(&format!("failed reading nzb body: {e}")),
    };

    let filename = url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("download.nzb");

    match job::stage_and_create(state, filename, category, bytes).await {
        Ok(new_job) => {
            let id = new_job.id.to_string();
            {
                let mut store = state.jobs.write().await;
                store.enqueue(new_job);
                let _ = store.save();
            }
            ok_response(json!({"status": true, "nzo_ids": [id]}))
        }
        Err(e) => error_response(&format!("failed to process nzb: {e}")),
    }
}
