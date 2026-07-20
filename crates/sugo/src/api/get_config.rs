use axum::response::Response;
use serde_json::json;

use crate::state::SharedState;

use super::ok_response;

/// Sonarr/Radarr's "Test" step for a SABnzbd download client calls this
/// during setup, mainly to list categories — lists the real configured
/// ones (`[[web.categories]]`), always including `"*"` first (every job
/// without an explicit category lands there, and it's never itself listed
/// in config).
pub async fn handle(state: &SharedState) -> Response {
    let config = state.config.read().await;
    let mut categories = vec![json!({"name": "*", "dir": "", "priority": 0})];
    categories.extend(
        config.web.categories.iter().map(
            |c| json!({"name": c.name, "dir": c.dir.clone().unwrap_or_default(), "priority": 0}),
        ),
    );
    ok_response(json!({
        "config": {
            "misc": {
                "complete_dir": "",
            },
            "categories": categories,
        }
    }))
}
