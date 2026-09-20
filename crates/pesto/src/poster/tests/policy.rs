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
