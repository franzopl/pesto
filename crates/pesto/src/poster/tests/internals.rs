use super::result::record_failure;
use super::*;

// ── Message-ID domain ─────────────────────────────────────────────────────

#[test]
fn message_id_domain_is_random() {
    let a = crate::article::generate_message_id(None);
    let b = crate::article::generate_message_id(None);
    assert_ne!(a, b);
    assert!(a.contains('@'));
    assert!(!a.contains("blocknews") && !a.contains("pesto"));
}

// ── physical_core_count ───────────────────────────────────────────────────

#[test]
fn physical_core_count_is_at_least_one() {
    assert!(parmesan::physical_core_count() >= 1);
}

#[test]
fn encode_concurrency_is_min_of_cores_and_connections() {
    assert_eq!(encode_concurrency(4, 8), 4);
    assert_eq!(encode_concurrency(4, 50), 4);
    assert_eq!(encode_concurrency(2, 8), 2);
    assert_eq!(encode_concurrency(1, 1), 1);
    assert_eq!(encode_concurrency(6, 8), 6);
    assert_eq!(encode_concurrency(16, 8), 8);
    assert_eq!(encode_concurrency(6, 2), 2);
}

#[test]
fn ready_queue_matches_nyuu_article_buffer() {
    assert_eq!(ready_queue_depth(8), 6);
    assert_eq!(ready_queue_depth(50), 25);
    assert_eq!(ready_queue_depth(1), 4);
}

// ── Shared buffer pool ────────────────────────────────────────────────────

fn minimal_shared(article_size: usize) -> Arc<Shared> {
    use crate::config::{FileConfig, Overrides};
    let mut file = FileConfig::default();
    file.posting.groups = Some(vec!["alt.test".into()]);
    let mut config = Config::resolve(
        file,
        Overrides {
            dry_run: Some(true),
            par2: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    config.article_size = article_size;
    let post_group = pick_post_group(&config.groups);
    Arc::new(Shared {
        config,
        servers: Arc::new(vec![]),
        results: Arc::new(Mutex::new(Vec::new())),
        failures: Mutex::new(Vec::new()),
        failed_tasks: Mutex::new(Vec::new()),
        events: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        paused: Arc::new(AtomicBool::new(false)),
        resume: None,
        resume_path: None,
        spool_dir: None,
        pool: Arc::new(Mutex::new(Vec::new())),
        encode_pool: Arc::new(Mutex::new(Vec::new())),
        total_retries: std::sync::atomic::AtomicUsize::new(0),
        post_group,
        release_prefix: None,
        release_from: None,
        run_id: 0,
        total_files: 0,
        check_tx: Mutex::new(None),
    })
}

#[test]
fn buffer_pool_reuses_released_buffer() {
    let shared = minimal_shared(1024);
    let buf = shared.try_acquire_buffer(1024).unwrap();
    let cap = buf.capacity();
    shared.release_buffer(buf);
    let buf2 = shared.try_acquire_buffer(1024).unwrap();
    // Reused buffer has at least the same capacity as the released one.
    assert!(buf2.capacity() >= cap);
    assert_eq!(buf2.len(), 1024);
}

#[test]
fn buffer_pool_drops_oversized_buffers() {
    // article_size = 100; a buffer with capacity > 200 must not be pooled.
    let shared = minimal_shared(100);
    let big = vec![0u8; 300]; // capacity >> article_size * 2
    shared.release_buffer(big);
    // Pool should be empty — allocates fresh on next acquire.
    assert!(shared.pool.lock().unwrap().is_empty());
}

#[test]
fn buffer_pool_acquire_fresh_when_empty() {
    let shared = minimal_shared(512);
    let buf = shared.try_acquire_buffer(256).unwrap();
    assert_eq!(buf.len(), 256);
}

// ── record_failure ────────────────────────────────────────────────────────

#[test]
fn record_failure_appends_description() {
    let shared = minimal_shared(1024);
    let path = std::path::PathBuf::from("ep.mkv");
    let meta = meta_with_name(&path, "ep.mkv");
    let task = PostTask {
        meta: Arc::new(meta),
        part: 2,
        total: 5,
        offset: 0,
        data: vec![],
        subject_name: "ep.mkv".into(),
        yenc_name: "ep.mkv".into(),
        from: String::new(),
        date: (None, None),
        file_crc32: None,
    };
    record_failure(&shared, &task.meta, &task, "<mid@host>".into(), "timeout");
    let failures = shared.failures.lock().unwrap();
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("ep.mkv"));
    assert!(failures[0].contains("2/5"));
    assert!(failures[0].contains("timeout"));
    // The original Message-ID is preserved for the same-ID end-of-run retry.
    let tasks = shared.failed_tasks.lock().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].message_id, "<mid@host>");
}
