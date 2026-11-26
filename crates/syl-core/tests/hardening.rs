//! Regression tests for the path-traversal, UTF-8 truncation, container-name,
//! shell-quoting, mid-turn compaction, unanswered tool_use, mid-stream error
//! and cancellable-backoff fixes.
use std::path::Path;

use serde_json::json;
use syl_core::api::compaction::{choose_keep_boundary, KeepBoundary};

const WINDOW: usize = 10;
use syl_core::api::types::{ContentBlock, Message, MessageContent};
use syl_core::container::name_for as container_name_for;
use syl_core::tools::memory::{memory_read, memory_write};
use syl_core::tools::workflow::{load_workflow, workflow_delete, workflow_write};
use syl_core::util::{is_safe_key, shell_quote, truncate_output, MAX_TOOL_OUTPUT};

// ---------------------------------------------------------------------------
// Key validation / path traversal
// ---------------------------------------------------------------------------

#[test]
fn safe_keys_accept_normal_names() {
    for k in ["ship", "deploy-staging", "project_stack", "v1.2", "ABC123"] {
        assert!(is_safe_key(k), "{k} should be allowed");
    }
}

#[test]
fn safe_keys_reject_traversal_and_junk() {
    for k in ["", "..", "../x", "a/b", "/etc/passwd", ".hidden", "a..b", "a b", "a\\b", "x\0y"] {
        assert!(!is_safe_key(k), "{k:?} should be rejected");
    }
    assert!(!is_safe_key(&"a".repeat(129)));
}

#[test]
fn memory_write_cannot_escape_memory_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let mem = tmp.path().join("memory");
    let r = memory_write(&mem, "../escaped", "pwned");
    assert!(r.starts_with("Error: invalid memory key"), "{r}");
    assert!(!tmp.path().join("escaped.md").exists());

    let r = memory_read(&mem, "../escaped");
    assert!(r.starts_with("Error: invalid memory key"), "{r}");
}

#[test]
fn workflow_tools_cannot_escape_workflow_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let wf = tmp.path().join("workflows");
    let content = "# X\n- Step: `echo hi`\n";

    let r = workflow_write(&wf, "../escaped", content);
    assert!(r.starts_with("Error: invalid workflow name"), "{r}");
    assert!(!tmp.path().join("escaped.md").exists());

    // A file outside the dir must not be deletable or loadable via traversal.
    std::fs::write(tmp.path().join("victim.md"), content).unwrap();
    let r = workflow_delete(&wf, "../victim");
    assert!(r.starts_with("Error: invalid workflow name"), "{r}");
    assert!(tmp.path().join("victim.md").exists());
    assert!(load_workflow(&wf, "../victim").is_none());
}

// ---------------------------------------------------------------------------
// UTF-8-safe truncation
// ---------------------------------------------------------------------------

#[test]
fn truncate_output_does_not_panic_on_multibyte_boundary() {
    // 'é' is 2 bytes; prefix with one ASCII byte so MAX_TOOL_OUTPUT lands mid-char.
    let s = format!("a{}", "é".repeat(MAX_TOOL_OUTPUT));
    assert!(!s.is_char_boundary(MAX_TOOL_OUTPUT));
    let out = truncate_output(&s);
    assert!(out.contains("[truncated:"));
    let body = out.split("\n... [truncated").next().unwrap();
    assert!(body.len() <= MAX_TOOL_OUTPUT);
}

#[test]
fn truncate_output_handles_emoji_and_box_drawing() {
    for unit in ["🙂", "─", "漢"] {
        let s = unit.repeat(MAX_TOOL_OUTPUT);
        let _ = truncate_output(&s); // must not panic
    }
}

// ---------------------------------------------------------------------------
// Shell quoting
// ---------------------------------------------------------------------------

#[test]
fn shell_quote_survives_spaces_and_quotes() {
    for raw in ["/Users/me/My Projects/syl", "it's", "a'b'c", "$(rm -rf /)", ""] {
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("printf %s {}", shell_quote(raw))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), raw);
    }
}

// ---------------------------------------------------------------------------
// Per-project container names
// ---------------------------------------------------------------------------

#[test]
fn container_names_are_stable_and_per_project() {
    let a1 = container_name_for(Path::new("/Users/me/code/a"));
    let a2 = container_name_for(Path::new("/Users/me/code/a"));
    let b = container_name_for(Path::new("/Users/me/code/b"));
    assert_eq!(a1, a2);
    assert_ne!(a1, b);
    assert!(a1.starts_with("syl-agent-"));
    assert_eq!(a1.len(), "syl-agent-".len() + 10);
    assert!(a1.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
}

// ---------------------------------------------------------------------------
// Mid-turn compaction boundary
// ---------------------------------------------------------------------------

fn tool_use(id: &str) -> Message {
    Message::assistant_blocks(vec![ContentBlock::ToolUse {
        id: id.to_string(),
        name: "run_bash".to_string(),
        input: json!({"command": "true"}),
    }])
}

fn tool_result(id: &str) -> Message {
    Message {
        role: "user".to_string(),
        content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "ok".to_string(),
            is_error: None,
        }]),
    }
}

#[test]
fn boundary_keeps_the_whole_window_not_just_the_last_prompt() {
    // A long tool turn, then a new prompt. The kept tail must start at the
    // window, inside that turn, not jump forward to the new prompt (which
    // would keep almost nothing).
    let mut msgs = vec![Message::user_text("start")];
    for i in 0..15 {
        msgs.push(tool_use(&format!("t{i}")));
        msgs.push(tool_result(&format!("t{i}")));
    }
    msgs.push(Message::user_text("next question"));
    let start = msgs.len() - WINDOW;
    assert_eq!(msgs[start].role, "user", "window starts on a tool result");
    let b = choose_keep_boundary(&msgs, start).unwrap();
    assert_eq!(b, KeepBoundary::Assistant(start + 1));

    // On a plain user message, it starts right there.
    let b = choose_keep_boundary(&msgs, msgs.len() - 1).unwrap();
    assert_eq!(b, KeepBoundary::UserText(msgs.len() - 1));
}

#[test]
fn boundary_inside_long_tool_chain_starts_at_assistant() {
    // One user prompt followed by a long tool chain: no plain user message in
    // the recent window. Old code kept nothing here and lost the whole chain.
    let mut msgs = vec![Message::user_text("do a big task")];
    for i in 0..20 {
        msgs.push(tool_use(&format!("t{i}")));
        msgs.push(tool_result(&format!("t{i}")));
    }
    let b = choose_keep_boundary(&msgs, msgs.len() - WINDOW).unwrap();
    let KeepBoundary::Assistant(i) = b else { panic!("expected Assistant, got {b:?}") };
    assert!(i >= msgs.len() - WINDOW);
    assert_eq!(msgs[i].role, "assistant");
    // Every tool_result in the tail is preceded by its tool_use.
    assert_eq!(msgs[i + 1].role, "user");
}

// ---------------------------------------------------------------------------
// Unanswered tool_use (turn cut off by max_tokens)
// ---------------------------------------------------------------------------

#[test]
fn unanswered_tool_use_is_not_stored() {
    use syl_core::api::compaction::drop_unanswered_tool_uses;
    use syl_core::api::types::StopReason;

    let text = ContentBlock::Text { text: "let me check".into(), cache_control: None };
    let tu = ContentBlock::ToolUse { id: "t1".into(), name: "run_bash".into(), input: json!({}) };

    // A tool_use turn keeps everything: its tools get run and answered.
    let both = vec![text.clone(), tu.clone()];
    assert_eq!(drop_unanswered_tool_uses(&both, &StopReason::ToolUse), both);

    // Cut off mid tool call: the tool_use would never get a tool_result.
    assert_eq!(drop_unanswered_tool_uses(&both, &StopReason::MaxTokens), vec![text]);

    // Nothing but the tool_use (plus thinking) left: placeholder text, never empty.
    let thinking = ContentBlock::Thinking { thinking: "x".into(), signature: Some("s".into()) };
    let out = drop_unanswered_tool_uses(&[thinking.clone(), tu], &StopReason::MaxTokens);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], thinking);
    assert!(matches!(out[1], ContentBlock::Text { .. }));
}

// ---------------------------------------------------------------------------
// Mid-stream error events
// ---------------------------------------------------------------------------

async fn mock_server_with(bodies: Vec<&'static str>) -> wiremock::MockServer {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let n = bodies.len();
    for (i, body) in bodies.into_iter().enumerate() {
        let m = Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"));
        // Earlier bodies answer once each; the last one answers the rest.
        let m = if i + 1 < n { m.up_to_n_times(1) } else { m };
        m.mount(&server).await;
    }
    server
}

const OVERLOADED_MID_STREAM: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
    "event: error\n",
    "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
);

const OK_STREAM: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"complete\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
);

fn mock_config(server: &wiremock::MockServer) -> syl_core::config::AppConfig {
    let mut config = syl_core::config::AppConfig::default();
    config.api_url = format!("{}/v1/messages", server.uri());
    config.api_key = "test-key".to_string();
    config
}

#[tokio::test]
async fn overloaded_error_mid_stream_is_retried_not_returned_as_partial_turn() {
    use syl_core::api::client::{call_with_retry_shared, SystemBlock};
    let server = mock_server_with(vec![OVERLOADED_MID_STREAM, OK_STREAM]).await;
    let state = syl_core::shared::SharedState::new();
    let turn = call_with_retry_shared(
        &reqwest::Client::new(),
        &mock_config(&server),
        &state,
        &[Message::user_text("hi")],
        &[SystemBlock::text("sys")],
        tokio_util::sync::CancellationToken::new(),
        |_| {},
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(turn.text(), Some("complete"));
}

#[tokio::test]
async fn a_reply_cut_off_mid_stream_is_marked_as_restarted() {
    use syl_core::api::client::{call_with_retry_shared, BlockDeltaEvent, SystemBlock};
    let server = mock_server_with(vec![OVERLOADED_MID_STREAM, OK_STREAM]).await;
    let state = syl_core::shared::SharedState::new();
    let deltas = std::sync::Mutex::new(Vec::new());
    let notices = std::sync::Mutex::new(Vec::new());
    call_with_retry_shared(
        &reqwest::Client::new(),
        &mock_config(&server),
        &state,
        &[Message::user_text("hi")],
        &[SystemBlock::text("sys")],
        tokio_util::sync::CancellationToken::new(),
        |n| notices.lock().unwrap().push(n.to_string()),
        |d| deltas.lock().unwrap().push(match d {
            BlockDeltaEvent::TextDelta { text } => text,
            BlockDeltaEvent::Restart => "<restart>".to_string(),
            _ => String::new(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(deltas.into_inner().unwrap(), ["partial", "<restart>", "complete"]);
    let notices = notices.into_inner().unwrap();
    // Said even though the backoff is short.
    assert!(notices.iter().any(|n| n.contains("partway through the reply; it is discarded")), "{notices:?}");
}

#[tokio::test]
async fn a_retry_before_anything_was_shown_is_not_a_restart() {
    use syl_core::api::client::{call_with_retry_shared, BlockDeltaEvent, SystemBlock};
    use wiremock::matchers::method;
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(529).set_body_string("{}"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(OK_STREAM, "text/event-stream"))
        .mount(&server)
        .await;
    let state = syl_core::shared::SharedState::new();
    let restarts = std::sync::atomic::AtomicUsize::new(0);
    call_with_retry_shared(
        &reqwest::Client::new(),
        &mock_config(&server),
        &state,
        &[Message::user_text("hi")],
        &[SystemBlock::text("sys")],
        tokio_util::sync::CancellationToken::new(),
        |_| {},
        |d| {
            if matches!(d, BlockDeltaEvent::Restart) {
                restarts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        },
    )
    .await
    .unwrap();
    assert_eq!(restarts.into_inner(), 0);
}

#[tokio::test]
async fn cancel_while_an_error_body_stalls_returns_promptly() {
    use syl_core::api::client::{stream_turn, CancelledError, SystemBlock};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A 429 whose headers arrive but whose body never does.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = sock.read(&mut buf).await;
        let _ = sock
            .write_all(b"HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{")
            .await;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        drop(sock);
    });
    let config = syl_core::config::AppConfig {
        api_url: format!("http://{addr}/v1/messages"),
        api_key: "test-key".into(),
        ..Default::default()
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop.cancel();
    });
    let started = std::time::Instant::now();
    let err = stream_turn(&reqwest::Client::new(), &config, &[Message::user_text("hi")], &[SystemBlock::text("sys")], cancel, &|_| {})
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<CancelledError>().is_some(), "{err}");
    assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?}", started.elapsed());
}

#[tokio::test]
async fn other_error_mid_stream_is_surfaced() {
    use syl_core::api::client::{stream_turn, SystemBlock};
    const API_ERROR: &str = concat!(
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}\n\n",
    );
    let server = mock_server_with(vec![API_ERROR]).await;
    let err = stream_turn(
        &reqwest::Client::new(),
        &mock_config(&server),
        &[Message::user_text("hi")],
        &[SystemBlock::text("sys")],
        tokio_util::sync::CancellationToken::new(),
        &|_| {},
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(err.contains("api_error") && err.contains("boom"), "{err}");
}

#[tokio::test]
async fn cancel_during_retry_backoff_returns_promptly() {
    use syl_core::api::client::{call_with_retry_shared, SystemBlock};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(529)).mount(&server).await;

    let cancel = tokio_util::sync::CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        c.cancel();
    });
    let started = std::time::Instant::now();
    let state = syl_core::shared::SharedState::new();
    let err = call_with_retry_shared(
        &reqwest::Client::new(),
        &mock_config(&server),
        &state,
        &[Message::user_text("hi")],
        &[SystemBlock::text("sys")],
        cancel,
        |_| {},
        |_| {},
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("cancelled"), "{err}");
    // Full backoff would be 1 + 2 + 4 + 8 + 16 + 30 s.
    assert!(started.elapsed().as_secs() < 2, "took {:?}", started.elapsed());
}

// ---------------------------------------------------------------------------
// HTML stripping (used to panic: the regex used an unsupported backreference)
// ---------------------------------------------------------------------------

#[test]
fn html_tool_output_is_stripped_without_panicking() {
    let page = "<!DOCTYPE html><html><head><style>body { color: red }</style>\
                <script type=\"text/javascript\">var x = '<b>';</script></head>\
                <body><h1>Title</h1><p>Hello   world</p><SCRIPT>evil()</SCRIPT></body></html>";
    let out = syl_core::util::process_tool_output(page);
    assert_eq!(out, "Title Hello world");
}
