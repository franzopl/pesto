//! Issue #57: a `producer` error (bad PAR2 geometry, a memory-budget check,
//! file I/O, …) used to be indistinguishable from a real user cancellation —
//! both just set `PostOutcome::cancelled = true`, so a caller had no way to
//! tell "the user pressed Ctrl-C" apart from "the run failed, and here's
//! why". Since the failure is a deterministic function of the file and the
//! config, the same file then fails identically on every retry, with no clue
//! that retrying won't help. `PostOutcome::failure_reason` fixes that by
//! carrying the actual error message through.

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

#[tokio::test(flavor = "multi_thread")]
async fn producer_error_is_reported_via_failure_reason_not_a_bare_cancellation() {
    let dir = std::env::temp_dir().join(format!("pesto_producer_error_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");

    // A tiny PAR2 slice size relative to the file size pushes the input slice
    // count past the PAR2 spec's 32768 limit (see `producer`'s
    // `total_slices > 32768` check), which is a deterministic function of
    // the file size and this config — exactly the "same file fails every
    // time" pattern from the issue.
    const FILE_SIZE: usize = 3_000_000;
    std::fs::write(&input, vec![0u8; FILE_SIZE]).unwrap();

    let config = Config {
        host: "unused".to_string(),
        port: 563,
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 65536,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        proxy: None,
        proxy_check_ip: false,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 10,
        par2_slice_size: Some(64),
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
        memory_limit: None,
        par2_temp_dir: None,
        compress_temp_dir: None,
        par2_only: false,
        par2_before_upload: false,
        threads: 0,
        simd: pesto::par2::SimdPath::Auto,
        extra_servers: vec![],
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        compress_volume_size: None,
        nzb_title: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: vec![],
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
        tvdb_kind: None,
        mal_id: None,
        indexer_url: None,
        indexer_api_key: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: true,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        file_counter: false,
        message_id_domain: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: false,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 5,
        check_retries: 2,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        check_recover_percent: 15,
        check_recover_max: 0,
        pipeline_depth: 1,
        keepalive_interval: 0,
    };

    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();

    assert!(
        outcome.cancelled,
        "a producer error should still set `cancelled` (existing callers rely on it)"
    );
    assert!(
        outcome.segments.is_empty(),
        "nothing should have been posted when producer fails before queuing any article"
    );
    let reason = outcome
        .failure_reason
        .as_deref()
        .expect("producer error should populate failure_reason");
    assert!(
        reason.contains("too many input slices"),
        "failure_reason should carry the actual producer error, got: {reason}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
