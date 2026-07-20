use axum::response::Response;
use serde_json::json;

use crate::state::SharedState;

use super::ok_response;

pub async fn handle(state: &SharedState) -> Response {
    let store = state.jobs.read().await;
    let active_count = usize::from(store.active.is_some());
    ok_response(json!({
        "status": {
            "version": concat!("sugo-", env!("CARGO_PKG_VERSION")),
            "paused": store.paused,
            "noofslots_total": store.pending.len() + active_count,
        }
    }))
}
