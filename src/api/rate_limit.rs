use std::time::{SystemTime, UNIX_EPOCH};

pub const RATE_WINDOW_MS: u64 = 60_000;

#[derive(Debug, Clone, Default)]
pub struct SessionTokens {
    pub input:  u64,
    pub output: u64,
    pub calls:  u64,
}

impl SessionTokens {
    pub fn record_usage(&mut self, input_tokens: u64, output_tokens: u64) {
        self.input  += input_tokens;
        self.output += output_tokens;
        self.calls  += 1;
    }
}

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

/// Pre-call throttle. Mirrors `maybe-throttle!` from syl.bb.
///
/// If more than 80 % of the per-minute token budget is already consumed,
/// sleeps until the oldest in-window call falls out of the 60-second window,
/// then prunes and returns. This keeps Syl below the API rate limit without
/// relying on 429 responses.
///
/// `budget` is the tokens-per-minute limit from `AppConfig::rate_limit`.
pub async fn maybe_throttle(calls: &mut Vec<RateCall>, budget: u64) {
    prune_old_calls(calls);
    let used = window_tokens(calls);

    if used > (budget as f64 * 0.8) as u64 {
        if let Some(oldest) = calls.first() {
            let elapsed  = now_ms().saturating_sub(oldest.ts);
            let wait_ms  = RATE_WINDOW_MS.saturating_sub(elapsed);
            if wait_ms > 0 {
                tracing::info!(
                    wait_ms,
                    used_tokens = used,
                    budget,
                    "Token budget at 80% — cooling down"
                );
                println!(
                    "\n[Token budget] Cooling down for {}s to stay under rate limit...",
                    wait_ms / 1000
                );
                tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                prune_old_calls(calls);
            }
        }
    }
}
