use std::time::{SystemTime, UNIX_EPOCH};

pub const RATE_WINDOW_MS: u64 = 60_000;

#[derive(Debug, Clone)]
pub struct RateCall {
    pub ts:     u64,  // millis since epoch
    pub tokens: u64,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn record_rate_call(calls: &mut Vec<RateCall>, tokens: u64) {
    calls.push(RateCall { ts: now_ms(), tokens });
}

pub fn prune_old_calls(calls: &mut Vec<RateCall>) {
    let cutoff = now_ms().saturating_sub(RATE_WINDOW_MS);
    calls.retain(|c| c.ts > cutoff);
}

/// Return the total input tokens used within the current 60-second window.
pub fn window_tokens(calls: &[RateCall]) -> u64 {
    calls.iter().map(|c| c.tokens).sum()
}
