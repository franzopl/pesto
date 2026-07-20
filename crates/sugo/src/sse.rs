//! Server-Sent Events. Three streams:
//! - `/events/:job_id` — raw JSON [`crate::state::JobEvent`]s filtered to
//!   one job; not used by the UI today, kept as a programmatic surface.
//! - `/events/queue` — the dashboard's actual live-update mechanism: on
//!   *any* job-state change, re-renders the whole queue partial
//!   server-side and pushes the HTML directly, so htmx's `sse` extension
//!   can swap it in with zero custom JS (see the plan's Design Decision 1).
//! - `/events/notifications` — JSON, filtered to only `Finished` events,
//!   consumed by `templates/base.html`'s small inline toast script (the one
//!   place this crate uses custom JS — see Design Decision 2).

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use uuid::Uuid;

use crate::state::{JobEventPayload, SharedState};
use crate::web::dashboard::render_queue_html;

pub async fn handler(
    State(state): State<SharedState>,
    Path(job_id): Path<Uuid>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(move |msg| {
        let event = msg.ok()?;
        if event.job_id != job_id {
            return None;
        }
        let json = serde_json::to_string(&event).ok()?;
        Some(Ok(Event::default().event("job").data(json)))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

pub async fn queue_handler(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    // Any event at all — regardless of which job or what changed — is a
    // "something changed, re-render" signal; the whole-queue partial is
    // small enough that re-rendering it on every (already throttled at the
    // source, see `job::pipeline::FLUSH_INTERVAL`) tick is cheap.
    let stream = BroadcastStream::new(rx)
        .filter_map(|msg| msg.ok())
        .then(move |_event| {
            let state = state.clone();
            async move { Ok(Event::default().data(render_queue_html(&state).await)) }
        });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

pub async fn notifications_handler(
    State(state): State<SharedState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| {
        let event = msg.ok()?;
        match &event.payload {
            JobEventPayload::Finished { .. } => {
                let json = serde_json::to_string(&event).ok()?;
                Some(Ok(Event::default().data(json)))
            }
            JobEventPayload::Progress { .. } => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}
