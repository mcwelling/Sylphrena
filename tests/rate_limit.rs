/// Tests for api/rate_limit.rs (Phase 4 mandatory gate).
use syl_rs::api::rate_limit::{
    now_ms, prune_old_calls, record_rate_call, window_tokens, RateCall, RATE_WINDOW_MS,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn old_call(tokens: u64) -> RateCall {
    // Timestamp older than the window
    let ts = now_ms().saturating_sub(RATE_WINDOW_MS + 5_000);
    RateCall { ts, tokens }
}

fn recent_call(tokens: u64) -> RateCall {
    RateCall { ts: now_ms(), tokens }
}

// ---------------------------------------------------------------------------
// window_tokens
// ---------------------------------------------------------------------------

#[test]
fn window_tokens_empty() {
    assert_eq!(window_tokens(&[]), 0);
}

#[test]
fn window_tokens_sums_all() {
    let calls = vec![recent_call(1000), recent_call(2000), recent_call(500)];
    assert_eq!(window_tokens(&calls), 3500);
}

// ---------------------------------------------------------------------------
// prune_old_calls
// ---------------------------------------------------------------------------

#[test]
fn prune_removes_expired_calls() {
    let mut calls = vec![old_call(5000), recent_call(1000)];
    prune_old_calls(&mut calls);
    assert_eq!(calls.len(), 1, "expired call should be removed");
    assert_eq!(calls[0].tokens, 1000, "recent call should remain");
}

#[test]
fn prune_keeps_all_recent() {
    let mut calls = vec![recent_call(100), recent_call(200)];
    prune_old_calls(&mut calls);
    assert_eq!(calls.len(), 2);
}

#[test]
fn prune_all_expired_results_in_empty() {
    let mut calls = vec![old_call(999), old_call(888)];
    prune_old_calls(&mut calls);
    assert!(calls.is_empty());
}

// ---------------------------------------------------------------------------
// record_rate_call
// ---------------------------------------------------------------------------

#[test]
fn record_call_appends_and_increments() {
    let mut calls = Vec::new();
    record_rate_call(&mut calls, 3000);
    assert_eq!(calls.len(), 1);
    assert_eq!(window_tokens(&calls), 3000);

    record_rate_call(&mut calls, 1000);
    assert_eq!(calls.len(), 2);
    assert_eq!(window_tokens(&calls), 4000);
}

#[test]
fn record_call_sets_recent_timestamp() {
    let mut calls = Vec::new();
    let before = now_ms();
    record_rate_call(&mut calls, 500);
    let after = now_ms();
    assert!(calls[0].ts >= before && calls[0].ts <= after);
}

// ---------------------------------------------------------------------------
// Throttle threshold logic
// ---------------------------------------------------------------------------

/// Simulate the threshold check that `maybe_throttle` / `call_with_retry_shared` uses.
fn over_threshold(calls: &[RateCall], budget: u64) -> bool {
    let used = window_tokens(calls);
    used > (budget as f64 * 0.8) as u64
}

#[test]
fn under_threshold_no_throttle() {
    let budget = 60_000u64;
    let calls = vec![recent_call(10_000)]; // 10k / 60k = 16.7%
    assert!(!over_threshold(&calls, budget));
}

#[test]
fn at_exactly_80_pct_no_throttle() {
    let budget = 100_000u64;
    let calls = vec![recent_call(80_000)]; // exactly 80%
    // 80_000 > 80_000 is false, so no throttle
    assert!(!over_threshold(&calls, budget));
}

#[test]
fn above_80_pct_triggers_throttle() {
    let budget = 100_000u64;
    let calls = vec![recent_call(80_001)]; // 80.001%
    assert!(over_threshold(&calls, budget));
}

#[test]
fn at_100_pct_triggers_throttle() {
    let budget = 60_000u64;
    let calls = vec![recent_call(60_000)]; // 100%
    assert!(over_threshold(&calls, budget));
}

#[test]
fn expired_calls_excluded_from_threshold() {
    let budget = 60_000u64;
    // Two old calls that would put us over budget, plus a tiny recent one
    let mut calls = vec![old_call(50_000), old_call(50_000), recent_call(1_000)];
    prune_old_calls(&mut calls);
    // After pruning only the 1_000 token call remains
    assert!(!over_threshold(&calls, budget));
}
