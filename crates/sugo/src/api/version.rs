use axum::response::Response;
use serde_json::json;

use super::ok_response;

pub fn handle() -> Response {
    ok_response(json!({"version": concat!("sugo-", env!("CARGO_PKG_VERSION"))}))
}
