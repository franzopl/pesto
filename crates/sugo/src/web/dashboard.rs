use askama::Template;
use axum::extract::{Multipart, State};
use axum::response::Response;

use crate::job;
use crate::state::SharedState;

use super::render;

pub struct QueueRow {
    pub name: String,
    pub status: &'static str,
    pub percentage: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    queue: Vec<QueueRow>,
    paused: bool,
}

#[derive(Template)]
#[template(path = "partials/queue_list.html")]
struct QueueListTemplate {
    queue: Vec<QueueRow>,
}

async fn queue_rows(state: &SharedState) -> (Vec<QueueRow>, bool) {
    let store = state.jobs.read().await;
    let queue = store
        .active
        .iter()
        .chain(store.pending.iter())
        .map(|j| QueueRow {
            name: j.name.clone(),
            status: j.status.sabnzbd_label(),
            percentage: format!("{:.0}", j.percentage()),
        })
        .collect();
    (queue, store.paused)
}

pub async fn page(State(state): State<SharedState>) -> Response {
    let (queue, paused) = queue_rows(&state).await;
    render(DashboardTemplate { queue, paused })
}

/// Polled every few seconds by the queue table itself (`hx-trigger="every
/// 3s"`, see `templates/partials/queue_list.html`) to refresh progress
/// without a full page reload.
pub async fn queue_partial(State(state): State<SharedState>) -> Response {
    let (queue, _paused) = queue_rows(&state).await;
    render(QueueListTemplate { queue })
}

/// Browser upload form target: same staging path as `mode=addfile`
/// ([`crate::api::addfile`]), just without the SABnzbd JSON envelope —
/// returns the refreshed queue partial instead, matching htmx's
/// swap-the-response-into-the-DOM model.
pub async fn upload(State(state): State<SharedState>, mut multipart: Multipart) -> Response {
    let mut filename = "upload.nzb".to_string();
    let mut bytes: Option<Vec<u8>> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        if let Some(name) = field.file_name() {
            filename = name.to_string();
        }
        if let Ok(data) = field.bytes().await {
            bytes = Some(data.to_vec());
        }
    }

    if let Some(bytes) = bytes {
        if let Ok(new_job) = job::stage_and_create(&state, &filename, None, bytes).await {
            let mut store = state.jobs.write().await;
            store.enqueue(new_job);
            let _ = store.save();
        }
    }

    queue_partial(State(state)).await
}
