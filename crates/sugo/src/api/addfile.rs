use std::collections::HashMap;

use axum::extract::Multipart;
use axum::response::Response;
use serde_json::json;

use crate::job;
use crate::state::SharedState;

use super::{error_response, ok_response};

/// `mode=addfile` — multipart `.nzb` upload, the primary way `*arr` tooling
/// and manual browser uploads submit a release.
pub async fn handle(
    state: &SharedState,
    params: &HashMap<String, String>,
    mut multipart: Multipart,
) -> Response {
    let mut filename = "upload.nzb".to_string();
    let mut bytes: Option<Vec<u8>> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => return error_response(&format!("invalid multipart body: {e}")),
        };
        if let Some(name) = field.file_name() {
            filename = name.to_string();
        }
        match field.bytes().await {
            Ok(data) => bytes = Some(data.to_vec()),
            Err(e) => return error_response(&format!("failed reading upload: {e}")),
        }
    }

    let Some(bytes) = bytes else {
        return error_response("no .nzb file in request body");
    };
    let category = params.get("cat").cloned();

    match job::stage_and_create(state, &filename, category, bytes).await {
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
