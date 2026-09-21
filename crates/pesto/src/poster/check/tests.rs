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
