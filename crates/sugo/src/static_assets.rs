//! Vendored static assets, embedded into the binary via `include_bytes!` —
//! deliberately not a `rust-embed`-style crate for just three small files
//! (see the plan's Design Decision 5). `htmx.min.js`/`htmx-sse.js` are
//! htmx 2.0.4 (MIT) and its official SSE extension, fetched once from
//! `unpkg.com` and committed under `assets/` rather than pulled from a CDN
//! at request time — keeps the whole UI working offline and avoids a
//! runtime dependency on a third party staying up.

use axum::http::header;
use axum::response::{IntoResponse, Response};

const HTMX_JS: &[u8] = include_bytes!("../assets/htmx.min.js");
const HTMX_SSE_JS: &[u8] = include_bytes!("../assets/htmx-sse.js");
const STYLE_CSS: &[u8] = include_bytes!("../assets/style.css");

pub async fn htmx_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        HTMX_JS,
    )
        .into_response()
}

pub async fn htmx_sse_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        HTMX_SSE_JS,
    )
        .into_response()
}

pub async fn style_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}
