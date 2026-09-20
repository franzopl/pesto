use super::*;

// ── resolve_date ──────────────────────────────────────────────────────────

#[test]
fn resolve_date_none_omits_header() {
    assert_eq!(resolve_date(None), (None, None));
}

#[test]
fn resolve_date_now_returns_rfc2822() {
    let (d, ts) = resolve_date(Some("now"));
    let d = d.unwrap();
    // Should look like "Mon, 01 Jan 2024 00:00:00 +0000".
    assert!(d.ends_with("+0000"));
    assert!(d.contains(':'));
    assert!(ts.unwrap() > 0);
}

#[test]
fn resolve_date_random_returns_rfc2822() {
    let (d, ts) = resolve_date(Some("random"));
    let d = d.unwrap();
    assert!(d.ends_with("+0000"));
    assert!(ts.unwrap() > 0);
}

#[test]
fn resolve_date_random_within_2h() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (_, ts) = resolve_date(Some("random"));
    let ts = ts.unwrap();
    assert!(ts <= now, "random date must not be in the future");
    assert!(
        now - ts < 2 * 3600 + 1,
        "random date must be within the last 2 hours"
    );
}

#[test]
fn resolve_date_fixed_is_returned_verbatim() {
    let fixed = "Tue, 14 Jan 2025 10:00:00 +0000";
    let (d, ts) = resolve_date(Some(fixed));
    assert_eq!(d.as_deref(), Some(fixed));
    assert!(ts.is_none());
}

// ── RateLimiter ───────────────────────────────────────────────────────────

#[tokio::test]
async fn rate_limiter_zero_rate_never_sleeps() {
    let mut rl = RateLimiter::new(0);
    let start = Instant::now();
    rl.acquire(1_000_000).await;
    // Should return almost instantly (< 10 ms).
    assert!(start.elapsed() < Duration::from_millis(10));
}

#[tokio::test]
async fn rate_limiter_large_bucket_does_not_sleep_for_small_request() {
    // 10 MiB/s bucket, request 1 KiB — tokens are available immediately.
    let mut rl = RateLimiter::new(10 * 1024 * 1024);
    let start = Instant::now();
    rl.acquire(1024).await;
    assert!(start.elapsed() < Duration::from_millis(10));
}

// ── automatic recovery threshold (is_cheap_to_recover) ────────────────────

fn recover_config(check_recover_percent: u8, check_recover_max: usize) -> Config {
    let mut config = dry_run_config();
    config.check_recover_percent = check_recover_percent;
    config.check_recover_max = check_recover_max;
    config
}

#[test]
fn small_release_within_both_caps_is_cheap() {
    let config = recover_config(15, 50);
    // 3 missing out of 20 (15%) — right at the percent cap, under the max.
    assert!(is_cheap_to_recover(3, 20, &config));
}

#[test]
fn huge_release_capped_by_absolute_max_even_under_percent() {
    let config = recover_config(15, 50);
    // 15% of 100,000 is 15,000 — nowhere near cheap, even though it's
    // exactly the configured percentage.
    assert!(!is_cheap_to_recover(15_000, 100_000, &config));
}

#[test]
fn small_absolute_count_rejected_when_it_is_most_of_the_release() {
    let config = recover_config(15, 50);
    // 5 missing out of 10 (50%) — small in absolute terms, but a large
    // fraction of a tiny release looks systemic, not incidental.
    assert!(!is_cheap_to_recover(5, 10, &config));
}

#[test]
fn zero_missing_is_never_worth_recovering() {
    let config = recover_config(15, 50);
    assert!(!is_cheap_to_recover(0, 1000, &config));
}

#[test]
fn max_zero_disables_recovery_entirely() {
    let config = recover_config(100, 0);
    assert!(!is_cheap_to_recover(1, 2, &config));
}

// ── connection splitting (upload vs. streaming check) ─────────────────────

#[test]
fn split_connections_carves_auto_check_pool_out_of_the_total() {
    let mut config = dry_run_config();
    config.connections = 50;
    config.check_connections = 0; // auto
    let (check, upload) = split_connections(&config, true).unwrap();
    assert_eq!(check, 4);
    assert_eq!(upload, 46);
    assert_eq!(check + upload, 50);
}

#[test]
fn split_connections_disabled_uses_the_whole_total_for_upload() {
    let mut config = dry_run_config();
    config.connections = 50;
    let (check, upload) = split_connections(&config, false).unwrap();
    assert_eq!(check, 0);
    assert_eq!(upload, 50);
}

#[test]
fn split_connections_n1_check_auto_is_a_startup_error() {
    // T22: `check_connections == 0` is auto, not off. Auto on `-n 1`
    // would carve check down to 0 — fail loud instead of silently skipping.
    let mut config = dry_run_config();
    config.connections = 1;
    config.check_connections = 0; // auto
    config.check = true;
    let err = split_connections(&config, true).unwrap_err().to_string();
    assert!(
        err.contains("--no-check"),
        "start-up error must mention --no-check: {err}"
    );
    assert!(
        err.contains("-n") || err.contains("connections"),
        "start-up error must mention raising -n: {err}"
    );
}

#[test]
fn split_connections_explicit_is_carved_out_not_additive() {
    // T18: explicit N is no longer added on top of `-n`.
    let mut config = dry_run_config();
    config.connections = 10;
    config.check_connections = 4;
    let (check, upload) = split_connections(&config, true).unwrap();
    assert_eq!((check, upload), (4, 6));

    config.check_connections = 10;
    let (check, upload) = split_connections(&config, true).unwrap();
    assert_eq!(
        (check, upload),
        (9, 1),
        "-n 10 --check-connections 10 must clamp to 9+1, never 10+1"
    );

    config.connections = 1;
    config.check_connections = 1;
    let err = split_connections(&config, true).unwrap_err().to_string();
    assert!(
        err.contains("--no-check"),
        "-n 1 --check-connections 1 must error, not open 1+1: {err}"
    );
}

#[test]
fn split_connections_small_total_leaves_upload_at_least_one() {
    let mut config = dry_run_config();
    config.connections = 2;
    config.check_connections = 0; // auto
    let (check, upload) = split_connections(&config, true).unwrap();
    assert_eq!(check, 1);
    assert_eq!(upload, 1);
}

#[test]
fn split_connections_low_max_favors_upload_not_check() {
    // Regression guard: a flat "up to 4" auto check pool used to try to
    // reserve 3 out of a 4-connection total for checking, leaving
    // upload — the operation that actually matters — with just 1. The
    // auto pool must scale down with the total instead of staying flat.
    let mut config = dry_run_config();
    config.connections = 4;
    config.check_connections = 0; // auto
    let (check, upload) = split_connections(&config, true).unwrap();
    assert_eq!(check, 1);
    assert_eq!(upload, 3);
}

#[test]
fn nzb_write_decision_writes_when_everything_confirmed() {
    assert_eq!(
        nzb_write_decision(false, false, false, false),
        NzbWriteDecision::Write
    );
}

#[test]
fn nzb_write_decision_refuses_post_failures_even_with_allow() {
    assert_eq!(
        nzb_write_decision(true, false, false, true),
        NzbWriteDecision::Refuse
    );
}

#[test]
fn nzb_write_decision_allow_incomplete_unblocks_missing_only() {
    assert_eq!(
        nzb_write_decision(false, true, false, true),
        NzbWriteDecision::Write
    );
    assert_eq!(
        nzb_write_decision(false, true, false, false),
        NzbWriteDecision::Refuse
    );
}

#[test]
fn nzb_write_decision_inconclusive_always_refuses() {
    assert_eq!(
        nzb_write_decision(false, false, true, true),
        NzbWriteDecision::Refuse
    );
    assert_eq!(
        nzb_write_decision(false, true, true, true),
        NzbWriteDecision::Refuse
    );
}

// T16: season pack is a distinct artefact; incomplete episodes never merge.
#[test]
fn should_write_season_nzb_requires_every_episode_complete() {
    assert!(should_write_season_nzb(false, false, false));
    assert!(!should_write_season_nzb(true, false, false));
    assert!(!should_write_season_nzb(false, true, false));
    assert!(!should_write_season_nzb(false, false, true));
    assert!(!should_write_season_nzb(true, true, false));
    assert!(!should_write_season_nzb(false, true, true));
    assert!(!should_write_season_nzb(true, false, true));
    assert!(!should_write_season_nzb(true, true, true));
}

// T16: pack input is had_failures (true completeness), not
// nzb_write_decision. MissingConfirmed + --allow-incomplete-nzb writes
// the episode NZB but CLI/UpaPasta still pass incomplete=true.
#[test]
fn season_pack_uses_had_failures_not_nzb_write_decision() {
    let post_failures = false;
    let missing_confirmed = true;
    let inconclusive = false;
    let allow_incomplete_nzb = true;
    assert_eq!(
        nzb_write_decision(
            post_failures,
            missing_confirmed,
            inconclusive,
            allow_incomplete_nzb
        ),
        NzbWriteDecision::Write
    );
    // CLI UploadResult.had_failures / UploadOutcome.had_failures —
    // independent of the flag.
    let had_failures = post_failures || missing_confirmed || inconclusive;
    assert!(had_failures);
    assert!(!should_write_season_nzb(false, had_failures, false));
}

#[test]
fn split_connections_high_max_caps_the_auto_check_pool() {
    let mut config = dry_run_config();
    config.connections = 200;
    config.check_connections = 0; // auto
    let (check, upload) = split_connections(&config, true).unwrap();
    assert_eq!(check, 4);
    assert_eq!(upload, 196);
}
