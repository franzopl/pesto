//! Phase 2 of the memory-management work (`docs/memory-management.md`):
//! `--memory-limit`/`Config::memory_limit` is a *global* process budget, not
//! just PAR2's. This is the regression guard for the double-haircut trap
//! that design explicitly calls out: PAR2's share of the global ceiling must
//! actually constrain `--par2-memory-limit`, and it must do so without
//! silently collapsing to a needlessly tiny number by counting RLIMIT_AS
//! twice (see `Ceiling::effective_excluding_address_space`).
//!
//! Asserting on the *value* of the effective share is already covered by
//! fast unit tests in `memory::ceiling`/`memory::budget`; this test instead
//! asserts the end-to-end, observable behavior: a tiny global `memory_limit`
//! must cause a `par2_memory_limit` that doesn't fit inside PAR2's share to
//! be rejected up front, exactly like an over-tight RLIMIT_AS already was
//! before this phase.

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

#[tokio::test(flavor = "multi_thread")]
async fn tiny_global_memory_limit_rejects_an_oversized_par2_memory_limit() {
    let dir = std::env::temp_dir().join(format!(
        "pesto_global_memory_limit_bounds_par2_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");
    std::fs::write(&input, vec![0u8; 500_000]).unwrap();

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
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 10,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        // Explicit PAR2-stage limit, deliberately larger than PAR2's 60%
        // share of the tiny global ceiling below.
        par2_memory_limit: Some(2_000_000),
        // Global ceiling small enough that even PAR2's 60% share
        // (~600_000 bytes) can't fit the explicit par2_memory_limit above —
        // regardless of this test host's own RLIMIT_AS, which is why the
        // assertion below checks for the shared "won't fit safely" wording
        // rather than which specific source (RLIMIT_AS vs this global
        // ceiling) turned out to be the binding one.
        memory_limit: Some(1_000_000),
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
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: vec![],
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
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
        "a producer budget error should still set `cancelled`"
    );
    let reason = outcome
        .failure_reason
        .as_deref()
        .expect("the oversized par2_memory_limit should have been rejected up front");
    assert!(
        reason.contains("won't fit safely"),
        "expected a budget-rejection message, got: {reason}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
