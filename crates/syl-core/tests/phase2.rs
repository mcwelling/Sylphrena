/// Phase 2 tests: AppConfig, SseParser, wiremock API client.
use tempfile::TempDir;

use syl_core::api::client::SseParser;
use syl_core::config::{
    build_config, parse_cli_args, AppConfig, DEFAULT_RATE_LIMIT,
};

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

fn tmp_syl_dir() -> TempDir {
    tempfile::tempdir().unwrap()
}

// ---------------------------------------------------------------------------
// 1. AppConfig defaults
// ---------------------------------------------------------------------------

#[test]
fn test_appconfig_default_rate_limit() {
    let cfg = AppConfig::default();
    assert_eq!(cfg.rate_limit, DEFAULT_RATE_LIMIT);
}

// ---------------------------------------------------------------------------
// 2. build_config: CLI overrides defaults
// ---------------------------------------------------------------------------

#[test]
fn test_build_config_cli_rate_limit_override() {
    let dir = tmp_syl_dir();
    let cli = parse_cli_args(&["--rate-limit", "30000"]);
    let cfg = build_config(&cli, dir.path());
    assert_eq!(cfg.rate_limit, 30_000);
}

#[test]
fn test_build_config_cli_thinking_budget() {
    let dir = tmp_syl_dir();
    let cli = parse_cli_args(&["--thinking", "5000"]);
    let cfg = build_config(&cli, dir.path());
    assert_eq!(cfg.thinking_budget, Some(5000));
}

#[test]
fn test_build_config_cli_timeout_converts_to_ms() {
    let dir = tmp_syl_dir();
    let cli = parse_cli_args(&["--timeout", "30"]);
    let cfg = build_config(&cli, dir.path());
    assert_eq!(cfg.timeout_ms, 30_000);
}

#[test]
fn test_build_config_flags() {
    let dir = tmp_syl_dir();
    let cli = parse_cli_args(&["--resume", "--fresh", "--no-banner", "--light"]);
    let cfg = build_config(&cli, dir.path());
    assert!(cfg.resume);
    assert!(cfg.fresh);
    assert!(cfg.no_banner);
    assert!(cfg.light);
}

// ---------------------------------------------------------------------------
// 3. build_config: TOML file is loaded
// ---------------------------------------------------------------------------

#[test]
fn test_build_config_from_toml() {
    let dir = tmp_syl_dir();
    let toml = "model = \"claude-sonnet-4-6\"\nrate_limit = 45000\ntheme = \"light\"\n";
    std::fs::write(dir.path().join("config.toml"), toml).unwrap();

    let cli = parse_cli_args(&[]);
    let cfg = build_config(&cli, dir.path());
    assert_eq!(cfg.model, "claude-sonnet-4-6");
    assert_eq!(cfg.rate_limit, 45_000);
    assert!(cfg.light); // theme = "light" sets cfg.light
}

// ---------------------------------------------------------------------------
// 4. SseParser — bytes-level line buffering
// ---------------------------------------------------------------------------

#[test]
fn test_sse_parser_single_event() {
    let mut p = SseParser::new();
    let bytes = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
    let events = p.feed(bytes);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["type"], "ping");
}

#[test]
fn test_sse_parser_split_across_chunks() {
    let mut p = SseParser::new();
    // First chunk ends mid-line
    let first = b"event: content_block_start\ndata: {\"type\":\"content_block_start\"";
    let second = b",\"index\":0}\n\n";
    let events1 = p.feed(first);
    assert!(events1.is_empty(), "no complete event yet");
    let events2 = p.feed(second);
    assert_eq!(events2.len(), 1);
    assert_eq!(events2[0].data["type"], "content_block_start");
    assert_eq!(events2[0].data["index"], 0);
}

#[test]
fn test_sse_parser_multiple_events_single_chunk() {
    let mut p = SseParser::new();
    let bytes = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0}\n\n",
    );
    let events = p.feed(bytes.as_bytes());
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data["type"], "message_start");
    assert_eq!(events[1].data["type"], "content_block_start");
}

#[test]
fn test_sse_parser_skips_done_sentinel() {
    let mut p = SseParser::new();
    let bytes = b"data: [DONE]\n\n";
    let events = p.feed(bytes);
    assert!(events.is_empty());
}

#[test]
fn test_sse_parser_skips_invalid_json() {
    let mut p = SseParser::new();
    let bytes = b"data: not-json\n\ndata: {\"type\":\"ok\"}\n\n";
    let events = p.feed(bytes);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["type"], "ok");
}

// ---------------------------------------------------------------------------
// 6. SseParser + StreamHandler round-trip (end-to-end SSE parsing)
// ---------------------------------------------------------------------------

fn full_sse_response(text: &str) -> Vec<u8> {
    let events = format!(
        concat!(
            "event: message_start\n",
            "data: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":10,\"output_tokens\":0}}}}}}\n\n",
            "event: content_block_start\n",
            "data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
            "event: content_block_delta\n",
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n",
            "event: content_block_stop\n",
            "data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
            "event: message_delta\n",
            "data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n",
            "event: message_stop\n",
            "data: {{\"type\":\"message_stop\"}}\n\n",
        ),
        text = text
    );
    events.into_bytes()
}

#[test]
fn test_sse_full_round_trip() {
    use syl_core::api::sse::StreamHandler;
    use syl_core::api::types::StopReason;

    let mut parser = SseParser::new();
    let mut handler = StreamHandler::new();

    let bytes = full_sse_response("hello world");
    for event in parser.feed(&bytes) {
        handler.on_event(&event.data);
    }
    let result = handler.build_result();

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.text(), Some("hello world"));
    assert_eq!(result.usage.input_tokens, 10);
    assert_eq!(result.usage.output_tokens, 5);
}

#[test]
fn test_sse_tool_use_round_trip() {
    use syl_core::api::sse::StreamHandler;
    use syl_core::api::types::{ContentBlock, StopReason};

    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"run_bash\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"ls\\\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":15}}\n\n",
    );

    let mut parser = SseParser::new();
    let mut handler = StreamHandler::new();
    for event in parser.feed(sse.as_bytes()) {
        handler.on_event(&event.data);
    }
    let result = handler.build_result();

    assert_eq!(result.stop_reason, StopReason::ToolUse);
    let tools = result.tool_uses();
    assert_eq!(tools.len(), 1);
    match tools[0] {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "tu_1");
            assert_eq!(name, "run_bash");
            assert_eq!(input["command"], "ls");
        }
        _ => panic!("expected ToolUse block"),
    }
}

// ---------------------------------------------------------------------------
// 7. wiremock: stream_turn against a mock HTTP server
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_stream_turn_with_mock_server() {
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use syl_core::api::client::{stream_turn, SystemBlock};
    use syl_core::api::types::{Message, StopReason};
    use syl_core::config::AppConfig;

    let server = MockServer::start().await;

    let sse_body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"mocked\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let mut config = AppConfig::default();
    config.api_url = format!("{}/v1/messages", server.uri());
    config.api_key = "test-key".to_string();

    let client = reqwest::Client::new();
    let messages = vec![Message::user_text("hello")];
    let system = vec![SystemBlock::text("You are helpful.")];
    let cancel = CancellationToken::new();

    let result = stream_turn(&client, &config, &messages, &system, cancel, &|_| {})
        .await
        .unwrap();

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.text(), Some("mocked"));
    assert_eq!(result.usage.input_tokens, 5);
    assert_eq!(result.usage.output_tokens, 3);
}

#[tokio::test]
async fn test_stream_turn_retries_on_429() {
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use syl_core::api::client::SystemBlock;
    use syl_core::api::types::{Message, StopReason};
    use syl_core::config::AppConfig;

    let server = MockServer::start().await;

    let sse_body = concat!(
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

    // First request → 429, second → 200 with valid SSE
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse_body, "text/event-stream"))
        .mount(&server)
        .await;

    let mut config = AppConfig::default();
    config.api_url = format!("{}/v1/messages", server.uri());
    config.api_key = "test-key".to_string();

    let client = reqwest::Client::new();
    let messages = vec![Message::user_text("retry test")];
    let system = vec![SystemBlock::text("sys")];
    let cancel = CancellationToken::new();

    // call_with_retry_shared should succeed on second attempt
    let state = syl_core::shared::SharedState::new();
    let result = syl_core::api::client::call_with_retry_shared(
        &client, &config, &state, &messages, &system, cancel, |_| {}, |_| {},
    )
    .await
    .unwrap();

    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.text(), Some("ok"));
}

#[tokio::test]
async fn test_stream_turn_propagates_non_retryable_error() {
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use syl_core::api::client::{call_with_retry_shared, SystemBlock};
    use syl_core::api::types::Message;
    use syl_core::config::AppConfig;

    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&server)
        .await;

    let mut config = AppConfig::default();
    config.api_url = format!("{}/v1/messages", server.uri());
    config.api_key = "test-key".to_string();

    let client = reqwest::Client::new();
    let messages = vec![Message::user_text("test")];
    let system = vec![SystemBlock::text("sys")];
    let cancel = CancellationToken::new();

    let state = syl_core::shared::SharedState::new();
    let result =
        call_with_retry_shared(&client, &config, &state, &messages, &system, cancel, |_| {}, |_| {})
            .await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("400"), "expected 400 in error: {}", msg);
}

#[tokio::test]
async fn test_stream_turn_cancel() {
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use syl_core::api::client::{stream_turn, SystemBlock};
    use syl_core::api::types::Message;
    use syl_core::config::AppConfig;

    let server = MockServer::start().await;

    // Server sends a very slow response (hangs for 1 second before sending anything)
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_raw("data: {\"type\":\"message_stop\"}\n\n", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut config = AppConfig::default();
    config.api_url = format!("{}/v1/messages", server.uri());
    config.api_key = "test-key".to_string();

    let client = reqwest::Client::new();
    let messages = vec![Message::user_text("cancel test")];
    let system = vec![SystemBlock::text("sys")];
    let cancel = CancellationToken::new();

    // Cancel almost immediately
    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel_clone.cancel();
    });

    let result = stream_turn(&client, &config, &messages, &system, cancel, &|_| {}).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cancelled"));
}
