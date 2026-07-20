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
    let bytes_left = job.total_bytes.saturating_sub(job.bytes_done);
    let mb = job.total_bytes as f64 / 1_048_576.0;
    let mb_left = bytes_left as f64 / 1_048_576.0;
    let timeleft = job
        .eta_seconds
        .map(|s| pesto::ui::render::format_duration(s as f64))
        .unwrap_or_else(|| "0:00:00".to_string());
    json!({
        "nzo_id": job.id.to_string(),
        "filename": job.name,
        "cat": job.category,
        "status": job.status.sabnzbd_label(),
        "priority": "Normal",
        "mb": format!("{mb:.2}"),
        "mbleft": format!("{mb_left:.2}"),
        "size": pesto::progress::format_size(job.total_bytes),
        "sizeleft": pesto::progress::format_size(bytes_left),
        "percentage": format!("{:.0}", job.percentage()),
        "timeleft": timeleft,
    })
}
