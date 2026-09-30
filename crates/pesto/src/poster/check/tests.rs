use super::*;

#[test]
fn fast_repost_withheld_below_the_sample_floor() {
    // Even a 0% miss rate shouldn't be trusted with almost no data —
    // a single miss out of 3 checks is not distinguishable from a
    // systemic problem yet.
    assert!(!should_fast_repost(3, 1));
    assert!(!should_fast_repost(MIN_SAMPLE_FOR_FAST_REPOST - 1, 0));
}

#[test]
fn fast_repost_allowed_once_sample_floor_met_with_a_low_rate() {
    // 1 miss in 20 checks (5%) sits right at the threshold — allowed.
    assert!(should_fast_repost(MIN_SAMPLE_FOR_FAST_REPOST, 1));
    // A single isolated miss in a much larger, otherwise-clean run.
    assert!(should_fast_repost(1000, 5));
}

#[test]
fn fast_repost_withheld_once_the_rate_looks_systemic() {
    // 2 misses in 20 checks (10%) is over the 5% threshold.
    assert!(!should_fast_repost(MIN_SAMPLE_FOR_FAST_REPOST, 2));
    // A third of checks missing is a server having a bad time, not a
    // handful of unlucky articles.
    assert!(!should_fast_repost(300, 100));
}

fn err(msg: &str) -> anyhow::Error {
    anyhow::anyhow!("{msg}")
}

#[test]
fn post_refusal_is_441_and_other_4xx_except_auth() {
    assert!(is_post_refusal(&err(
        "article rejected by server (441): 435 Already exists in history"
    )));
    assert!(is_post_refusal(&err(
        "POST not permitted: 440 Posting Not Allowed"
    )));
    assert!(is_post_refusal(&err(
        "unexpected POST response: 441 article rejected"
    )));
    assert!(!is_post_refusal(&err(
        "authentication rejected by server (code 502); check the configured username and password"
    )));
    assert!(!is_post_refusal(&err(
        "authentication rejected by server (code 481); check the configured username and password"
    )));
    assert!(!is_post_refusal(&err(
        "authentication rejected by server (code 482); check the configured username and password"
    )));
    assert!(!is_post_refusal(&err(
        "POST not permitted: 480 Authentication required"
    )));
    assert!(!is_post_refusal(&err(
        "unexpected POST response: 481 Authentication failed"
    )));
    assert!(!is_post_refusal(&err(
        "unexpected POST response: 502 Permission denied"
    )));
    assert!(!is_post_refusal(&err("connection reset by peer")));
    assert!(!is_post_refusal(&err("timed out")));
}

fn test_inner(results: Arc<Mutex<Vec<PostedSegment>>>) -> Inner {
    use crate::config::{Config, FileConfig, Overrides};
    let mut file = FileConfig::default();
    file.posting.groups = Some(vec!["alt.test".into()]);
    let config = Config::resolve(
        file,
        Overrides {
            dry_run: Some(true),
            par2: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    Inner {
        heaps: vec![Mutex::new(BinaryHeap::new())],
        in_flight: AtomicUsize::new(0),
        open: AtomicBool::new(true),
        config,
        groups: vec!["alt.test".into()],
        results,
        still_missing: Mutex::new(Vec::new()),
        inconclusive: Mutex::new(Vec::new()),
        events: None,
        cancel: None,
        servers: Arc::new(Vec::new()),
        checked_count: AtomicUsize::new(0),
        reposted_count: AtomicUsize::new(0),
        first_checks: AtomicUsize::new(0),
        first_misses: AtomicUsize::new(0),
        resume: None,
        encryption_adapter: None,
    }
}

#[test]
fn splice_preserves_logical_identity_and_subject_ordinals() {
    let id = crate::poster::outcome::SegmentIdentity::checked(0, 1, 2, 1).unwrap();
    let initial = PostedSegment {
        file_name: "file.bin".into(),
        file_path: Arc::from(std::path::Path::new("file.bin")),
        subject_name: Arc::from("file.bin"),
        wire_name: Arc::from("file.bin"),
        wire_yenc_name: Arc::from("file.bin"),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<orig@test>".into(),
        bytes: 100,
        from: Arc::from("p@x"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 1,
        total_files: 2,
        segment_identity: Some(id),
    };
    let results = Arc::new(Mutex::new(vec![initial]));
    let inner = test_inner(Arc::clone(&results));

    let replacement = PostedSegment {
        file_name: "file.bin".into(),
        file_path: Arc::from(std::path::Path::new("file.bin")),
        subject_name: Arc::from("file.bin"),
        wire_name: Arc::from("fresh_wire"),
        wire_yenc_name: Arc::from("fresh_yenc"),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<fresh@test>".into(),
        bytes: 100,
        from: Arc::from("p@x"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 1,
        total_files: 2,
        segment_identity: Some(id),
    };

    inner.splice(&replacement).unwrap();
    let r = results.lock().unwrap();
    assert_eq!(r[0].message_id, "<fresh@test>");
    assert_eq!(r[0].segment_identity, Some(id));
    assert_eq!(r[0].file_index, 1);
    assert_eq!(r[0].total_files, 2);
}

#[test]
fn splice_errors_if_repost_mutates_segment_identity() {
    let id1 = crate::poster::outcome::SegmentIdentity::checked(0, 1, 2, 1).unwrap();
    let id2 = crate::poster::outcome::SegmentIdentity::checked(1, 2, 2, 1).unwrap();
    let initial = PostedSegment {
        file_name: "file.bin".into(),
        file_path: Arc::from(std::path::Path::new("file.bin")),
        subject_name: Arc::from("file.bin"),
        wire_name: Arc::from("file.bin"),
        wire_yenc_name: Arc::from("file.bin"),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<orig@test>".into(),
        bytes: 100,
        from: Arc::from("p@x"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 1,
        total_files: 2,
        segment_identity: Some(id1),
    };
    let results = Arc::new(Mutex::new(vec![initial]));
    let inner = test_inner(Arc::clone(&results));

    let replacement = PostedSegment {
        file_name: "file.bin".into(),
        file_path: Arc::from(std::path::Path::new("file.bin")),
        subject_name: Arc::from("file.bin"),
        wire_name: Arc::from("fresh_wire"),
        wire_yenc_name: Arc::from("fresh_yenc"),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<fresh@test>".into(),
        bytes: 100,
        from: Arc::from("p@x"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 1,
        total_files: 2,
        segment_identity: Some(id2), // MUTATED!
    };

    let res = inner.splice(&replacement);
    assert!(res.is_err());
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("check repost altered logical segment identity"));
}

#[test]
fn splice_errors_if_repost_mutates_file_index() {
    let id = crate::poster::outcome::SegmentIdentity::checked(0, 1, 2, 1).unwrap();
    let initial = PostedSegment {
        file_name: "file.bin".into(),
        file_path: Arc::from(std::path::Path::new("file.bin")),
        subject_name: Arc::from("file.bin"),
        wire_name: Arc::from("file.bin"),
        wire_yenc_name: Arc::from("file.bin"),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<orig@test>".into(),
        bytes: 100,
        from: Arc::from("p@x"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 1,
        total_files: 2,
        segment_identity: Some(id),
    };
    let results = Arc::new(Mutex::new(vec![initial]));
    let inner = test_inner(Arc::clone(&results));

    let replacement = PostedSegment {
        file_name: "file.bin".into(),
        file_path: Arc::from(std::path::Path::new("file.bin")),
        subject_name: Arc::from("file.bin"),
        wire_name: Arc::from("fresh_wire"),
        wire_yenc_name: Arc::from("fresh_yenc"),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<fresh@test>".into(),
        bytes: 100,
        from: Arc::from("p@x"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 99, // MUTATED!
        total_files: 2,
        segment_identity: Some(id),
    };

    let res = inner.splice(&replacement);
    assert!(res.is_err());
    assert!(res
        .unwrap_err()
        .to_string()
        .contains("check repost altered file index or total files"));
}
