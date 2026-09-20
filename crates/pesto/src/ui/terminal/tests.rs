use super::*;
use crate::progress::FileEntry;
use crate::ui::render::visible_len;

pub(super) fn started_state(width_samples: bool) -> RenderState {
    let mut state = RenderState::new();
    state.apply(ProgressEvent::Started {
        mode: RunMode::Post,
        files: vec![FileEntry {
            name: "Movie.2026/movie.mkv".to_string(),
            segments: 785,
            bytes: 600_000_000,
        }],
        connections: 7,
        check_connections: 1,
        target: Some("news.example.com:563".to_string()),
        par2_bytes_hint: 60_000_000,
        par2_segments_hint: 79,
    });
    for _ in 0..300 {
        state.apply(ProgressEvent::SegmentDone {
            file: "Movie.2026/movie.mkv".to_string(),
            bytes: 768_000,
            ok: true,
        });
    }
    if width_samples {
        state.push_speed_sample(50_000_000.0);
        state.push_speed_sample(60_000_000.0);
    }
    state
}

#[path = "tests/renderer.rs"]
mod renderer;

#[path = "tests/state.rs"]
mod state;

#[path = "tests/outcomes.rs"]
mod outcomes;
