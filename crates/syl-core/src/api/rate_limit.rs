//! Rate limits: what the API reports, and an optional cap of Syl's own.
//!
//! Every Messages API response carries the organisation's limits in
//! `anthropic-ratelimit-*` headers: per bucket (input tokens, output
//! tokens), the limit per minute, what remains, and when the bucket is full
//! again. The buckets refill continuously, so before a request Syl waits
//! only as long as the bucket needs to hold the request, and only when the
//! last response said it nearly couldn't. A 429 says how long to wait in
//! `retry-after`.
//!
//! `rate_limit` in the config is a separate, optional cap (tokens a minute)
//! for when you want Syl to use less than the API allows: a sliding window
//! of the tokens sent.
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::HeaderMap;

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

/// One of the API's token buckets, as a response reported it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    /// Tokens per minute.
    pub limit: u64,
    /// Left when the response was sent.
    pub remaining: u64,
    /// When it is full again (ms since epoch).
    pub reset_ms: u64,
    /// When this was seen (ms since epoch).
    pub seen_ms: u64,
}

impl Bucket {
    /// What is left at `now`, counting the refill since it was seen.
    pub fn available(&self, now: u64) -> u64 {
        if now >= self.reset_ms {
            return self.limit;
        }
        let refilled = now.saturating_sub(self.seen_ms).saturating_mul(self.limit) / RATE_WINDOW_MS;
        self.remaining.saturating_add(refilled).min(self.limit)
    }

    /// How long until `need` tokens are available (zero if they are now).
    /// Never longer than until the bucket is full.
    pub fn wait_for(&self, need: u64, now: u64) -> Duration {
        let have = self.available(now);
        let need = need.min(self.limit);
        if have >= need || self.limit == 0 {
            return Duration::ZERO;
        }
        let ms = (need - have).saturating_mul(RATE_WINDOW_MS).div_ceil(self.limit);
        Duration::from_millis(ms.min(self.reset_ms.saturating_sub(now)))
    }
}

/// The API's limits from the last response.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ApiLimits {
    pub input: Option<Bucket>,
    pub output: Option<Bucket>,
}

impl ApiLimits {
    /// Read the `anthropic-ratelimit-*` headers. None when there are none
    /// (a proxy, or a test server).
    pub fn from_headers(headers: &HeaderMap, now: u64) -> Option<Self> {
        let num = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<u64>().ok();
        let bucket = |kind: &str| {
            let limit = num(&format!("anthropic-ratelimit-{kind}-limit"))?;
            let remaining = num(&format!("anthropic-ratelimit-{kind}-remaining"))?;
            let reset_ms = headers
                .get(format!("anthropic-ratelimit-{kind}-reset"))
                .and_then(|v| v.to_str().ok())
                .and_then(parse_rfc3339_ms)
                .unwrap_or(now + RATE_WINDOW_MS);
            Some(Bucket { limit, remaining, reset_ms, seen_ms: now })
        };
        let limits = ApiLimits {
            // Older or combined limits report one `tokens` bucket.
            input: bucket("input-tokens").or_else(|| bucket("tokens")),
            output: bucket("output-tokens"),
        };
        (limits.input.is_some() || limits.output.is_some()).then_some(limits)
    }

    /// How long to wait before sending about `input` uncached input tokens
    /// and up to `output` output tokens.
    pub fn wait_for(&self, input: u64, output: u64, now: u64) -> Duration {
        let i = self.input.map_or(Duration::ZERO, |b| b.wait_for(input, now));
        let o = self.output.map_or(Duration::ZERO, |b| b.wait_for(output, now));
        i.max(o)
    }
}

/// `retry-after`, in seconds (the API's form) or as an HTTP date (ignored).
pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers.get("retry-after")?.to_str().ok()?.trim();
    v.parse::<f64>().ok().filter(|s| s.is_finite() && *s >= 0.0).map(Duration::from_secs_f64)
}

/// `2026-11-25T10:00:30Z` (fractions and `+hh:mm` offsets too) as ms since
/// the epoch.
pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (time, offset_min) = if let Some(t) = rest.strip_suffix(['Z', 'z']) {
        (t, 0)
    } else {
        let i = rest.rfind(['+', '-'])?;
        let (t, off) = rest.split_at(i);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':')?;
        (t, sign * (oh.parse::<i64>().ok()? * 60 + om.parse::<i64>().ok()?))
    };
    let mut t = time.splitn(3, ':');
    let (hh, mm) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let sec: f64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let (yy, mm_) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm_ + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 - offset_min * 60;
    let ms = secs as f64 * 1000.0 + sec * 1000.0;
    (ms >= 0.0).then_some(ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn dates_parse() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("2024-02-29T12:00:00.5Z"), Some(1_709_208_000_500));
        assert_eq!(parse_rfc3339_ms("2024-02-29T13:00:00+01:00"), Some(1_709_208_000_000));
        assert_eq!(parse_rfc3339_ms("nope"), None);
    }

    #[test]
    fn headers_become_buckets() {
        let mut h = HeaderMap::new();
        for (k, v) in [
            ("anthropic-ratelimit-input-tokens-limit", "400000"),
            ("anthropic-ratelimit-input-tokens-remaining", "1000"),
            ("anthropic-ratelimit-input-tokens-reset", "1970-01-01T00:01:00Z"),
            ("anthropic-ratelimit-output-tokens-limit", "80000"),
            ("anthropic-ratelimit-output-tokens-remaining", "80000"),
            ("retry-after", "7"),
        ] {
            h.insert(k, HeaderValue::from_static(v));
        }
        let l = ApiLimits::from_headers(&h, 0).unwrap();
        assert_eq!(l.input, Some(Bucket { limit: 400_000, remaining: 1000, reset_ms: 60_000, seen_ms: 0 }));
        assert_eq!(l.output.unwrap().limit, 80_000);
        assert_eq!(retry_after(&h), Some(Duration::from_secs(7)));
        assert!(ApiLimits::from_headers(&HeaderMap::new(), 0).is_none());
    }

    #[test]
    fn waits_only_for_what_the_bucket_lacks() {
        // 60k a minute = 1 token a ms. 1k left, want 11k: 10s.
        let b = Bucket { limit: 60_000, remaining: 1000, reset_ms: 59_000, seen_ms: 0 };
        assert_eq!(b.wait_for(11_000, 0), Duration::from_millis(10_000));
        // Five seconds later it has refilled 5k.
        assert_eq!(b.wait_for(11_000, 5000), Duration::from_millis(5000));
        assert_eq!(b.wait_for(500, 0), Duration::ZERO);
        // Never past the reset, even for more than the limit.
        assert_eq!(b.wait_for(1_000_000, 0), Duration::from_millis(59_000));
        assert_eq!(b.available(60_000), 60_000);
    }
}
