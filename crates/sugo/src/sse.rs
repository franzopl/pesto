//! `/events/:job_id` — Server-Sent Events, filtered from the one global
//! [`crate::state::JobEvent`] broadcast down to a single job's updates. The
//! dashboard page subscribes to its active job's stream via htmx's `sse`
//! extension to update the progress bar live.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};
use uuid::Uuid;

use crate::state::SharedState;

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
