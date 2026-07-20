//! Server-rendered htmx pages: the dashboard/queue, history, and settings
//! views. Deliberately no JS build step — `askama` renders compile-time
//! checked templates under `templates/`, and htmx (vendored, see
//! [`crate::static_assets`]) handles interactivity on top.

pub mod dashboard;
pub mod history;
pub mod settings;

use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

pub(crate) fn render<T: Template>(template: T) -> Response {
    match template.render() {
        Ok(body) => Html(body).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "template render failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "template render error").into_response()
        }
    }
}
