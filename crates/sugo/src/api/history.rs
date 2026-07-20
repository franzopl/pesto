use std::collections::HashMap;

use axum::response::Response;
use serde_json::json;

use crate::job::Job;
use crate::state::SharedState;

use super::{error_response, ok_response};

pub async fn handle(state: &SharedState, params: &HashMap<String, String>) -> Response {
    if let Some(name) = params.get("name") {
        return command(state, name, params.get("value")).await;
    }
    list(state).await
}

async fn command(state: &SharedState, name: &str, value: Option<&String>) -> Response {
    match name {
        "delete" => {
            let Some(id) = value.and_then(|v| uuid::Uuid::parse_str(v).ok()) else {
                return error_response("invalid or missing value (nzo_id)");
            };
            let removed = state.jobs.write().await.remove_history(id);
            ok_response(json!({"status": removed}))
        }
        other => error_response(&format!("history command '{other}' not supported")),
    }
}

async fn list(state: &SharedState) -> Response {
    let store = state.jobs.read().await;
    let slots: Vec<_> = store.history.iter().map(slot_json).collect();
    ok_response(json!({
        "history": {
            "noofslots": slots.len(),
            "slots": slots,
        }
    }))
}

fn slot_json(job: &Job) -> serde_json::Value {
    json!({
        "nzo_id": job.id.to_string(),
        "name": job.name,
        "category": job.category,
        "status": job.status.sabnzbd_label(),
        "fail_message": job.message.clone().unwrap_or_default(),
        "storage": job.dest_dir.to_string_lossy(),
        "bytes": job.total_bytes,
        "size": pesto::progress::format_size(job.total_bytes),
    })
}
