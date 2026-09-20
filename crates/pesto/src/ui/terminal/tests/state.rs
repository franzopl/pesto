use super::*;

#[test]
fn started_event_seeds_persistent_state() {
    let state = started_state(false);

    assert!(state.started);
    assert_eq!(state.total_segments, 864);
    assert_eq!(state.total_bytes, 660_000_000);
    assert_eq!(state.conn_files.len(), 7);
    assert_eq!(state.check_connections, 1);
    assert_eq!(state.target.as_deref(), Some("news.example.com:563"));
}

#[test]
fn recovered_post_retry_repairs_failure_counters() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::SegmentDone {
        file: "Movie.2026/movie.mkv".to_string(),
        bytes: 768_000,
        ok: false,
    });
    state.apply(ProgressEvent::PostRetryRecovered {
        count: 1,
        previously_failed: true,
    });

    assert_eq!(state.failures, 0);
    assert_eq!(state.post_retry_pending, 0);
    assert_eq!(state.recovered_post_retries, 1);
}
