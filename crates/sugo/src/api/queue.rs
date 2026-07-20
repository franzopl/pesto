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
            let removed = state.jobs.write().await.remove_pending(id);
            ok_response(json!({"status": removed}))
        }
        "pause" => {
            state.jobs.write().await.set_paused(true);
            ok_response(json!({"status": true}))
        }
        "resume" => {
            state.jobs.write().await.set_paused(false);
            ok_response(json!({"status": true}))
        }
        other => error_response(&format!("queue command '{other}' not supported")),
    }
}

async fn list(state: &SharedState) -> Response {
    let store = state.jobs.read().await;
    let slots: Vec<_> = store
        .active
        .iter()
        .chain(store.pending.iter())
        .map(slot_json)
        .collect();
    ok_response(json!({
        "queue": {
            "status": if store.paused { "Paused" } else { "Downloading" },
            "noofslots": slots.len(),
            "noofslots_total": slots.len(),
            "slots": slots,
        }
    }))
}

fn slot_json(job: &Job) -> serde_json::Value {
    let mb = job.total_bytes as f64 / 1_048_576.0;
    let mb_left = job.total_bytes.saturating_sub(job.bytes_done) as f64 / 1_048_576.0;
    json!({
        "nzo_id": job.id.to_string(),
        "filename": job.name,
        "cat": job.category,
        "status": job.status.sabnzbd_label(),
        "priority": "Normal",
        "mb": format!("{mb:.2}"),
        "mbleft": format!("{mb_left:.2}"),
        "size": format!("{mb:.2} MB"),
        "sizeleft": format!("{mb_left:.2} MB"),
        "percentage": format!("{:.0}", job.percentage()),
        "timeleft": "0:00:00",
    })
}
