use askama::Template;
use axum::extract::State;
use axum::response::Response;

use crate::state::SharedState;

use super::render;

pub struct HistoryRow {
    pub name: String,
    pub status: &'static str,
    pub status_class: &'static str,
    pub message: String,
    pub size: String,
}

#[derive(Template)]
#[template(path = "history.html")]
struct HistoryTemplate {
    history: Vec<HistoryRow>,
}

pub async fn page(State(state): State<SharedState>) -> Response {
    let store = state.jobs.read().await;
    let history = store
        .history
        .iter()
        .map(|j| HistoryRow {
            name: j.name.clone(),
            status: j.status.sabnzbd_label(),
            status_class: j.status.css_class(),
            message: j.message.clone().unwrap_or_default(),
            size: pesto::progress::format_size(j.total_bytes),
        })
        .collect();
    drop(store);
    render(HistoryTemplate { history })
}
