use axum::response::Response;
use serde_json::json;

use super::ok_response;

/// Minimal stub: Sonarr/Radarr's "Test" step for a SABnzbd download client
/// calls this during setup (mainly to list categories). Real per-category
/// configuration is deferred — every job today uses category `"*"` (see
/// [`crate::job::stage_and_create`]) — but returning a well-formed, if
/// mostly empty, config keeps that autoconfigure handshake from failing.
pub fn handle() -> Response {
    ok_response(json!({
        "config": {
            "misc": {
                "complete_dir": "",
            },
            "categories": [
                {"name": "*", "dir": "", "priority": 0},
            ],
        }
    }))
}
