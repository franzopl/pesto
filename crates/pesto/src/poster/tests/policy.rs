use super::*;

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
