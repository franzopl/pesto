use askama::Template;
use axum::extract::{Multipart, State};
use axum::response::Response;

use crate::job::{self, Job};
use crate::state::SharedState;

use super::render;

/// Files beyond this many, per job, collapse into a "+N more" line instead
/// of each getting their own row — mirrors the cap `penne`'s own terminal
/// panel uses for the same reason (`ROADMAP.md`: "only the busiest 8 files
/// ever get their own bar").
const MAX_FILES_SHOWN: usize = 8;

pub struct QueueFileRow {
    pub name: String,
    pub percentage: String,
    pub done: bool,
}

pub struct QueueRow {
    pub id: String,
    pub name: String,
    pub status: &'static str,
    pub status_class: &'static str,
    pub percentage: String,
    pub bytes_text: String,
    pub speed_text: String,
    pub eta_text: String,
    /// Set only while `status == Verifying` — the PAR2 pass's live position
    /// in whichever file it's currently checking (see
    /// `job::JobVerifyProgress`'s doc comment for why this isn't a
    /// release-wide percentage).
    pub verify_text: Option<String>,
    pub files: Vec<QueueFileRow>,
    pub files_more: usize,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    queue: Vec<QueueRow>,
    paused: bool,
    categories: Vec<String>,
}

#[derive(Template)]
#[template(path = "partials/queue_list.html")]
struct QueueListTemplate {
    queue: Vec<QueueRow>,
}

fn job_to_row(j: &Job) -> QueueRow {
    let bytes_text = format!(
        "{} / {}",
        pesto::progress::format_size(j.bytes_done),
        pesto::progress::format_size(j.total_bytes)
    );
    let speed_text = if j.speed_bps > 1.0 {
        format!("{}/s", pesto::progress::format_size(j.speed_bps as u64))
    } else {
        String::new()
    };
    let eta_text = j
        .eta_seconds
        .map(|s| pesto::ui::render::format_duration(s as f64))
        .unwrap_or_default();
    let verify_text = j.verify.as_ref().map(|v| {
        format!(
            "verifying {}: {}/{} blocks",
            v.file_name, v.slices_done, v.total_slices
        )
    });
    let files: Vec<QueueFileRow> = j
        .files
        .iter()
        .take(MAX_FILES_SHOWN)
        .map(|f| QueueFileRow {
            name: f.name.clone(),
            percentage: format!("{:.0}", file_percentage(f)),
            done: f.done,
        })
        .collect();
    let files_more = j.files.len().saturating_sub(files.len());

    QueueRow {
        id: j.id.to_string(),
        name: j.name.clone(),
        status: j.status.sabnzbd_label(),
        status_class: j.status.css_class(),
        percentage: format!("{:.0}", j.percentage()),
        bytes_text,
        speed_text,
        eta_text,
        verify_text,
        files,
        files_more,
    }
}

fn file_percentage(f: &crate::job::JobFileProgress) -> f64 {
    if f.bytes_total == 0 {
        0.0
    } else {
        (f.bytes_done as f64 / f.bytes_total as f64 * 100.0).min(100.0)
    }
}

async fn queue_rows(state: &SharedState) -> (Vec<QueueRow>, bool) {
    let store = state.jobs.read().await;
    let queue = store
        .active
        .iter()
        .chain(store.pending.iter())
        .map(job_to_row)
        .collect();
    (queue, store.paused)
}

pub async fn page(State(state): State<SharedState>) -> Response {
    let (queue, paused) = queue_rows(&state).await;
    let categories = {
        let config = state.config.read().await;
        config
            .web
            .categories
            .iter()
            .map(|c| c.name.clone())
            .collect()
    };
    render(DashboardTemplate {
        queue,
        paused,
        categories,
    })
}

/// Rendered HTML for the queue partial alone — used both by the
/// `hx-get="/dashboard/queue"` fallback endpoint and by
/// [`crate::sse::queue_handler`], which pushes the same markup over SSE on
/// every job-state change instead of waiting for a poll.
pub async fn render_queue_html(state: &SharedState) -> String {
    let (queue, _paused) = queue_rows(state).await;
    QueueListTemplate { queue }.render().unwrap_or_default()
}

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
    let mut category: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("cat") {
            category = field.text().await.ok().filter(|s| !s.is_empty());
            continue;
        }
        if let Some(name) = field.file_name() {
            filename = name.to_string();
        }
        if let Ok(data) = field.bytes().await {
            bytes = Some(data.to_vec());
        }
    }

    if let Some(bytes) = bytes {
        if let Ok(new_job) = job::stage_and_create(&state, &filename, category, bytes).await {
            let (job_id, total_bytes) = (new_job.id, new_job.total_bytes);
            {
                let mut store = state.jobs.write().await;
                store.enqueue(new_job);
                let _ = store.save();
            }
            state.broadcast_queued(job_id, total_bytes);
        }
    }

    queue_partial(State(state)).await
}
