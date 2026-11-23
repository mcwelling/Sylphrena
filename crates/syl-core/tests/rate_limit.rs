/// Tests for api/rate_limit.rs (Phase 4 mandatory gate).
use syl_core::api::rate_limit::{
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
