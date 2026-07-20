use askama::Template;
use axum::extract::State;
use axum::response::Response;

use crate::state::SharedState;

use super::render;

pub struct HistoryRow {
    pub name: String,
    pub status: &'static str,
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
            message: j.message.clone().unwrap_or_default(),
            size: format!("{:.2} MB", j.total_bytes as f64 / 1_048_576.0),
        })
        .collect();
    drop(store);
    render(HistoryTemplate { history })
}
