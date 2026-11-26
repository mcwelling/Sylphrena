//! Rate limits: the API's headers and `retry-after` decide the waits.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use syl_core::api::client::{call_with_retry_shared, SystemBlock};
use syl_core::api::types::Message;
use syl_core::config::AppConfig;

const SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn config(server: &MockServer) -> AppConfig {
    AppConfig { api_url: format!("{}/v1/messages", server.uri()), api_key: "test-key".into(), ..AppConfig::default() }
}

fn rfc3339_in(ms: u64) -> String {
    let t = syl_core::api::rate_limit::now_ms() + ms;
    let s = syl_core::session::format_utc(t); // "YYYY-MM-DD HH:MM"
    format!("{}T{}:{:02}.{:03}Z", &s[..10], &s[11..16], (t / 1000) % 60, t % 1000)
}

async fn call(server: &MockServer, state: &std::sync::Arc<syl_core::shared::SharedState>, notices: &Mutex<Vec<String>>) {
    call_as(server, state, notices, &config(server).model).await
}

async fn call_as(
    server: &MockServer,
    state: &std::sync::Arc<syl_core::shared::SharedState>,
    notices: &Mutex<Vec<String>>,
    model: &str,
) {
    let cfg = AppConfig { model: model.to_string(), ..config(server) };
    call_with_retry_shared(
        &reqwest::Client::new(),
        &cfg,
        state,
        &[Message::user_text("hi")],
        &[SystemBlock::text("sys")],
        CancellationToken::new(),
        |n| notices.lock().unwrap().push(n.to_string()),
        |_| {},
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_429_waits_as_long_as_retry_after_says() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "2").set_body_string(
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE, "text/event-stream"))
        .mount(&server)
        .await;
    let state = syl_core::shared::SharedState::new();
    let notices = Mutex::new(Vec::new());
    let started = Instant::now();
    call(&server, &state, &notices).await;
    assert!(started.elapsed() >= Duration::from_millis(1900), "{:?}", started.elapsed());
    let notices = notices.into_inner().unwrap();
    assert!(notices.iter().any(|n| n.contains("rate limit was reached; retrying in 2s")), "{notices:?}");
}

#[tokio::test]
async fn a_nearly_empty_bucket_is_waited_for_and_only_as_long_as_it_needs() {
    let server = MockServer::start().await;
    // 60k a minute is 1 token a ms; nothing left, and the next request is
    // assumed to need at least 1000: about a second.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(SSE, "text/event-stream")
                .insert_header("anthropic-ratelimit-input-tokens-limit", "60000")
                .insert_header("anthropic-ratelimit-input-tokens-remaining", "0")
                .insert_header("anthropic-ratelimit-input-tokens-reset", rfc3339_in(60_000).as_str()),
        )
        .mount(&server)
        .await;
    let state = syl_core::shared::SharedState::new();
    let notices = Mutex::new(Vec::new());
    call(&server, &state, &notices).await;
    let limits = state.api_limits(&config(&server).model).expect("headers were read");
    assert_eq!(limits.input.unwrap().limit, 60_000);
    let started = Instant::now();
    call(&server, &state, &notices).await;
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(800) && took < Duration::from_secs(5), "{took:?}");
    let notices = notices.into_inner().unwrap();
    assert!(notices.iter().any(|n| n.contains("the API's per-minute token limit")), "{notices:?}");
}

#[tokio::test]
async fn without_a_cap_or_low_headers_nothing_waits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(SSE, "text/event-stream")
                .insert_header("anthropic-ratelimit-input-tokens-limit", "400000")
                .insert_header("anthropic-ratelimit-input-tokens-remaining", "390000")
                .insert_header("anthropic-ratelimit-input-tokens-reset", rfc3339_in(2_000).as_str()),
        )
        .mount(&server)
        .await;
    let state = syl_core::shared::SharedState::new();
    // Plenty of earlier traffic in the window: with no cap it doesn't matter.
    syl_core::api::rate_limit::record_rate_call(&mut state.recent_calls.lock().unwrap(), 10_000_000);
    let notices = Mutex::new(Vec::new());
    let started = Instant::now();
    for _ in 0..5 {
        call(&server, &state, &notices).await;
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(notices.into_inner().unwrap().is_empty());
}

const HAIKU: &str = "claude-haiku-4-5";

#[tokio::test]
async fn a_sub_agents_limits_do_not_pace_the_main_model() {
    let server = MockServer::start().await;
    // Haiku's bucket is empty; the main model's is nearly full.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(body_partial_json(serde_json::json!({ "model": HAIKU })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(SSE, "text/event-stream")
                .insert_header("anthropic-ratelimit-input-tokens-limit", "60000")
                .insert_header("anthropic-ratelimit-input-tokens-remaining", "0")
                .insert_header("anthropic-ratelimit-input-tokens-reset", rfc3339_in(60_000).as_str()),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(SSE, "text/event-stream")
                .insert_header("anthropic-ratelimit-input-tokens-limit", "400000")
                .insert_header("anthropic-ratelimit-input-tokens-remaining", "390000")
                .insert_header("anthropic-ratelimit-input-tokens-reset", rfc3339_in(2_000).as_str()),
        )
        .mount(&server)
        .await;
    let state = syl_core::shared::SharedState::new();
    let notices = Mutex::new(Vec::new());
    let main = config(&server).model;
    assert_ne!(main, HAIKU);

    call(&server, &state, &notices).await;
    // A dispatch_agent call on Haiku, then the main model again: no wait.
    call_as(&server, &state, &notices, HAIKU).await;
    let started = Instant::now();
    call(&server, &state, &notices).await;
    assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    assert!(notices.lock().unwrap().is_empty(), "{:?}", notices.lock().unwrap());

    // Each model keeps its own figures, for the status bar and /status.
    assert_eq!(state.api_limits(&main).unwrap().input.unwrap().limit, 400_000);
    assert_eq!(state.api_limits(HAIKU).unwrap().input.unwrap().limit, 60_000);

    // Haiku itself is still paced on its own bucket.
    let started = Instant::now();
    call_as(&server, &state, &notices, HAIKU).await;
    assert!(started.elapsed() >= Duration::from_millis(800), "{:?}", started.elapsed());
}

#[tokio::test]
async fn a_429_without_retry_after_waits_for_the_bucket_it_reports() {
    let server = MockServer::start().await;
    // 60k a minute is 1 token a ms; the bucket is empty and the request is
    // about 3,000 tokens, so 3s, where the plain backoff would be 1s.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("anthropic-ratelimit-input-tokens-limit", "60000")
                .insert_header("anthropic-ratelimit-input-tokens-remaining", "0")
                .insert_header("anthropic-ratelimit-input-tokens-reset", rfc3339_in(60_000).as_str())
                .set_body_string(r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE, "text/event-stream"))
        .mount(&server)
        .await;
    let state = syl_core::shared::SharedState::new();
    let notices = Mutex::new(Vec::new());
    let started = Instant::now();
    // A first request (nothing cached) of about 3,000 tokens.
    call_with_retry_shared(
        &reqwest::Client::new(),
        &config(&server),
        &state,
        &[Message::user_text("x".repeat(9_000))],
        &[SystemBlock::text("sys")],
        CancellationToken::new(),
        |n| notices.lock().unwrap().push(n.to_string()),
        |_| {},
    )
    .await
    .unwrap();
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(2800) && took < Duration::from_secs(6), "{took:?}");
    let notices = notices.into_inner().unwrap();
    assert!(notices.iter().any(|n| n.contains("retrying in 3s") || n.contains("retrying in 2s")), "{notices:?}");
}

#[tokio::test]
async fn a_wait_set_by_the_own_cap_says_so() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(SSE, "text/event-stream")
                .insert_header("anthropic-ratelimit-input-tokens-limit", "400000")
                .insert_header("anthropic-ratelimit-input-tokens-remaining", "400000")
                .insert_header("anthropic-ratelimit-input-tokens-reset", rfc3339_in(1_000).as_str()),
        )
        .mount(&server)
        .await;
    let state = syl_core::shared::SharedState::new();
    let notices = Mutex::new(Vec::new());
    call(&server, &state, &notices).await;
    // The API has plenty left, but the window is over 80% of a 10k cap.
    // The oldest call is backdated so the wait is short.
    {
        let mut calls = state.recent_calls.lock().unwrap();
        syl_core::api::rate_limit::record_rate_call(&mut calls, 9_000);
        let now = syl_core::api::rate_limit::now_ms();
        for c in calls.iter_mut() {
            c.ts = now - (syl_core::api::rate_limit::RATE_WINDOW_MS - 1_000);
        }
    }
    let cfg = AppConfig { rate_limit: 10_000, ..config(&server) };
    call_with_retry_shared(
        &reqwest::Client::new(),
        &cfg,
        &state,
        &[Message::user_text("hi")],
        &[SystemBlock::text("sys")],
        CancellationToken::new(),
        |n| notices.lock().unwrap().push(n.to_string()),
        |_| {},
    )
    .await
    .unwrap();
    let notices = notices.into_inner().unwrap();
    assert!(notices.iter().any(|n| n.contains("Syl's own rate_limit cap")), "{notices:?}");
}

mod estimate {
    use syl_core::api::client::{input_need, SystemBlock};
    use syl_core::api::types::Message;
    use syl_core::shared::ModelLimits;

    fn convo(n: usize, chars: usize) -> Vec<Message> {
        (0..n).map(|_| Message::user_text("x".repeat(chars))).collect()
    }

    const NOW: u64 = 10_000_000;

    #[test]
    fn a_warm_cache_charges_only_the_new_messages() {
        // Last request: 10 messages, 200k tokens in all, 1 minute ago.
        let l = ModelLimits { api: None, last_total: 200_000, last_ms: NOW - 60_000, last_count: 10 };
        let mut m = convo(10, 30_000);
        m.extend(convo(2, 6_000)); // the reply and a tool result: ~4k
        let need = input_need(&l, &m, &[], NOW);
        assert!((3_500..5_000).contains(&need), "{need}");
    }

    #[test]
    fn an_expired_cache_charges_everything_again() {
        let l = ModelLimits { api: None, last_total: 200_000, last_ms: NOW - 6 * 60_000, last_count: 10 };
        let mut m = convo(10, 30_000);
        m.extend(convo(2, 6_000));
        let need = input_need(&l, &m, &[], NOW);
        assert!((203_500..205_000).contains(&need), "{need}");
    }

    #[test]
    fn a_first_or_shortened_conversation_is_estimated_whole() {
        let sys = [SystemBlock::text("s".repeat(3_000))];
        let m = convo(3, 3_000);
        let first = input_need(&ModelLimits::default(), &m, &sys, NOW);
        assert!((3_900..4_200).contains(&first), "{first}");
        // After compaction the conversation is shorter than last time.
        let l = ModelLimits { api: None, last_total: 200_000, last_ms: NOW - 1_000, last_count: 40 };
        assert_eq!(input_need(&l, &m, &sys, NOW), first);
        // Never under 1,000.
        assert_eq!(input_need(&ModelLimits::default(), &convo(1, 10), &[], NOW), 1_000);
    }
}
