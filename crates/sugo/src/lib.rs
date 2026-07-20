//! SABnzbd-API-compatible web UI for [`penne`], the Usenet NZB downloader —
//! a separate crate consuming `penne` as a library, the same relationship
//! `upapasta` has with `pesto` (see the workspace `CLAUDE.md`'s
//! architecture principles and `penne`'s own `ROADMAP.md`, "Later — Web
//! UI"). Server-rendered (`askama` + htmx, no JS build step) rather than a
//! SPA, and exposes a subset of SABnzbd's real `/api` wire format so
//! existing tools (Sonarr, Radarr, Prowlarr, SAB-aware mobile apps) work
//! against it without new integration code.

pub mod api;
pub mod config;
pub mod job;
pub mod sse;
pub mod state;
pub mod static_assets;
pub mod web;

use axum::routing::{get, post};
use axum::Router;

use state::SharedState;

/// Builds the full application router. Split out from `src/bin/sugo.rs`
/// so integration tests can exercise it directly via `tower::ServiceExt::
/// oneshot`, without binding a real socket.
pub fn build_router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(web::dashboard::page))
        .route("/dashboard/queue", get(web::dashboard::queue_partial))
        .route("/dashboard/upload", post(web::dashboard::upload))
        .route("/history", get(web::history::page))
        .route("/settings", get(web::settings::page))
        .route("/settings/servers", post(web::settings::add_server))
        .route("/api", get(api::get_handler).post(api::post_handler))
        .route("/events/{job_id}", get(sse::handler))
        .route("/static/htmx.min.js", get(static_assets::htmx_js))
        .route("/static/htmx-sse.js", get(static_assets::htmx_sse_js))
        .route("/static/style.css", get(static_assets::style_css))
        .with_state(state)
}
