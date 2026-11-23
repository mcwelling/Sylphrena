//! End-to-end tests of the agent loop with no terminal: a wiremock model and
//! a stand-in podman, asserting on the event stream.
mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use syl_core::agent::{self, Agent, AgentCommand, AgentEvent, AgentHandle, SessionPaths, SessionStart, TurnEnd};
use syl_core::api::types::MessageContent;
use syl_core::session::{self, Entry, EntryKind, History, SessionInfo};
use syl_core::api::types::{ContentBlock, Message, StopReason, Usage};
use syl_core::config::AppConfig;
use syl_core::model::AnthropicAdapter;
use syl_core::shared::SharedState;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e))
        .collect()
}

fn tool_use_stream(id: &str, command: &str) -> String {
    let input = json!({ "command": command }).to_string();
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":20,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":"run_bash"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
    ])
}

fn text_stream(text: &str) -> String {
    text_stream_using(text, 30)
}

/// A text reply whose request used `input_tokens`.
fn text_stream_using(text: &str, input_tokens: u64) -> String {
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":input_tokens,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
    ])
}

/// Answer each POST with the next response, in order; the last repeats.
async fn model_server(responses: Vec<ResponseTemplate>) -> MockServer {
    let server = MockServer::start().await;
    let n = responses.len();
    for (i, r) in responses.into_iter().enumerate() {
        let m = Mock::given(method("POST")).respond_with(r);
        let m = if i + 1 < n { m.up_to_n_times(1) } else { m };
        m.mount(&server).await;
    }
    server
}

fn ok(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

struct Harness {
    handle: AgentHandle,
    state: Arc<SharedState>,
    tmp: tempfile::TempDir,
    /// The session the agent reported at startup.
    session: SessionInfo,
}

impl Harness {
    fn project(&self) -> std::path::PathBuf {
        self.tmp.path().join("project")
    }

    fn sessions_dir(&self) -> std::path::PathBuf {
        self.project().join(".syl/sessions")
    }

    /// The current chain of a session's log, read from disk.
    fn log(&self, id: &str) -> Vec<Entry> {
        session::load_entries(&self.sessions_dir(), id).unwrap()
    }

    fn send(&self, cmd: AgentCommand) {
        self.handle.commands.send(cmd).unwrap();
    }
}

fn test_config(server: &MockServer) -> AppConfig {
    AppConfig {
        api_url: format!("{}/v1/messages", server.uri()),
        api_key: "test-key".into(),
        timeout_ms: 10_000,
        ..Default::default()
    }
}

async fn start(server: &MockServer) -> Harness {
    start_with(test_config(server), tempfile::tempdir().unwrap(), SessionStart::New).await
}

/// Start an agent in `<tmp>/project` and wait for its first `SessionChanged`.
async fn start_with(config: AppConfig, tmp: tempfile::TempDir, start: SessionStart) -> Harness {
    let project = tmp.path().join("project");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    let state = SharedState::new();
    *state.current_model.lock().unwrap() = config.model.clone();
    let model = Arc::new(AnthropicAdapter::new(reqwest::Client::new(), state.clone()));
    let handle = agent::spawn(Agent {
        config,
        state: state.clone(),
        model,
        container: Arc::new(common::fake_container(&project, &bin)),
        paths: SessionPaths::for_project(&project),
        start,
    });
    let mut h = Harness {
        handle,
        state,
        tmp,
        session: SessionInfo {
            id: String::new(),
            name: None,
            message_count: 0,
            forked_from: None,
            resumed: false,
        },
    };
    h.session = next_session(&mut h).await;
    h
}

async fn next_event(h: &mut Harness) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(10), h.handle.events.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("agent stopped")
}

/// The next `SessionChanged`, failing on an `Error`.
async fn next_session(h: &mut Harness) -> SessionInfo {
    loop {
        match next_event(h).await {
            AgentEvent::SessionChanged { info } => return info,
            AgentEvent::Error { message } => panic!("session command failed: {message}"),
            _ => {}
        }
    }
}

/// Collect events until the next `TurnEnded` (inclusive).
async fn until_turn_ended(h: &mut Harness) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        let ev = next_event(h).await;
        let done = matches!(ev, AgentEvent::TurnEnded { .. });
        out.push(ev);
        if done {
            return out;
        }
    }
}

async fn request_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn full_tool_using_turn_runs_headless() {
    let server = model_server(vec![
        ok(tool_use_stream("tu_1", "echo hello-from-tool")),
        ok(text_stream("All done.")),
    ])
    .await;
    let mut h = start(&server).await;

    h.handle.commands.send(AgentCommand::Prompt("say hello".into())).unwrap();
    let events = until_turn_ended(&mut h).await;

    let finished_output = events.iter().find_map(|e| match e {
        AgentEvent::ToolCallFinished { output, .. } => Some(output.clone()),
        _ => None,
    });
    let output = finished_output.expect("a tool call finished");
    assert!(output.contains("hello-from-tool"), "{output}");

    let expected = vec![
        AgentEvent::TurnStarted,
        AgentEvent::RequestStarted,
        AgentEvent::Usage {
            request: Usage { input_tokens: 20, output_tokens: 7, ..Default::default() },
            session: Usage { input_tokens: 20, output_tokens: 7, ..Default::default() },
        },
        AgentEvent::ToolCallStarted {
            id: "tu_1".into(),
            name: "run_bash".into(),
            input: json!({ "command": "echo hello-from-tool" }),
        },
        AgentEvent::ToolCallFinished {
            id: "tu_1".into(),
            name: "run_bash".into(),
            output: output.clone(),
            is_error: false,
        },
        AgentEvent::RequestStarted,
        AgentEvent::TextDelta { text: "All done.".into() },
        AgentEvent::Usage {
            request: Usage { input_tokens: 30, output_tokens: 3, ..Default::default() },
            session: Usage { input_tokens: 50, output_tokens: 10, ..Default::default() },
        },
        AgentEvent::TurnEnded { reason: TurnEnd::Completed(StopReason::EndTurn) },
    ];
    assert_eq!(events, expected);

    // The second request answered the tool call.
    let bodies = request_bodies(&server).await;
    assert_eq!(bodies.len(), 2);
    let last = bodies[1]["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["type"], "tool_result");
    assert_eq!(last["content"][0]["tool_use_id"], "tu_1");

    // History is user, assistant(tool_use), user(tool_result), assistant(text),
    // and the log on disk derives the same history.
    let messages = h.state.messages.lock().unwrap().clone();
    assert_eq!(messages.len(), 4);
    let log = h.log(&h.session.id);
    assert_eq!(History::from_entries(&log).messages, messages);
    let kinds: Vec<&str> = log.iter().map(kind_name).collect();
    assert_eq!(
        kinds,
        ["session", "message", "model_change", "system_prompt", "message", "tool_result", "message"]
    );
    // The last request was exactly what the log says the model saw.
    assert_eq!(serde_json::to_value(&messages[..3]).unwrap(), bodies[1]["messages"]);
}

fn kind_name(e: &Entry) -> &'static str {
    match e.kind {
        EntryKind::Session { .. } => "session",
        EntryKind::SystemPrompt { .. } => "system_prompt",
        EntryKind::Message { .. } => "message",
        EntryKind::ToolResult { .. } => "tool_result",
        EntryKind::Compaction { .. } => "compaction",
        EntryKind::ModelChange { .. } => "model_change",
        EntryKind::Name { .. } => "name",
        EntryKind::Resumed => "resumed",
        EntryKind::Approval { .. } => "approval",
    }
}

#[tokio::test]
async fn cancel_stops_a_turn_in_flight() {
    let slow = ok(text_stream("too late")).set_delay(Duration::from_secs(30));
    let server = model_server(vec![slow]).await;
    let mut h = start(&server).await;

    h.handle.commands.send(AgentCommand::Prompt("hi".into())).unwrap();
    // Wait until the request is under way, then cancel.
    loop {
        match h.handle.events.recv().await.unwrap() {
            AgentEvent::RequestStarted => break,
            _ => continue,
        }
    }
    h.handle.commands.send(AgentCommand::Cancel).unwrap();

    let started = std::time::Instant::now();
    let events = until_turn_ended(&mut h).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnded { reason: TurnEnd::Cancelled }));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn api_error_ends_the_turn_with_an_error_event() {
    let server = model_server(vec![ResponseTemplate::new(400).set_body_string("bad request")]).await;
    let mut h = start(&server).await;

    h.handle.commands.send(AgentCommand::Prompt("hi".into())).unwrap();
    let events = until_turn_ended(&mut h).await;

    let n = events.len();
    assert!(
        matches!(&events[n - 2], AgentEvent::Error { message } if message.contains("400")),
        "{events:?}"
    );
    assert_eq!(events[n - 1], AgentEvent::TurnEnded { reason: TurnEnd::Failed });
}

#[tokio::test]
async fn set_model_applies_to_the_next_turn() {
    let server = model_server(vec![ok(text_stream("ok"))]).await;
    let mut h = start(&server).await;

    h.handle.commands.send(AgentCommand::SetModel("claude-haiku-4-5".into())).unwrap();
    h.handle.commands.send(AgentCommand::Prompt("hi".into())).unwrap();
    until_turn_ended(&mut h).await;

    let bodies = request_bodies(&server).await;
    assert_eq!(bodies[0]["model"], "claude-haiku-4-5");
}

#[tokio::test]
async fn prompts_sent_during_a_turn_run_after_it() {
    let first = ok(text_stream("first")).set_delay(Duration::from_millis(300));
    let server = model_server(vec![first, ok(text_stream("second"))]).await;
    let mut h = start(&server).await;

    h.handle.commands.send(AgentCommand::Prompt("one".into())).unwrap();
    h.handle.commands.send(AgentCommand::Prompt("two".into())).unwrap();

    let texts = |events: &[AgentEvent]| -> String {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(texts(&until_turn_ended(&mut h).await), "first");
    assert_eq!(texts(&until_turn_ended(&mut h).await), "second");

    let bodies = request_bodies(&server).await;
    let msgs = bodies[1]["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3, "one, first, two");
}

#[tokio::test]
async fn shutdown_stops_the_loop_even_while_a_sender_clone_is_alive() {
    // The TUI's Ctrl+C task keeps a clone of the command sender for the
    // whole session; exit used to wait for every sender to drop and hung.
    let server = model_server(vec![ok(text_stream("ok"))]).await;
    let h = start(&server).await;
    let _signal_handler_clone = h.handle.commands.clone();

    h.handle.commands.send(AgentCommand::Shutdown).unwrap();
    tokio::time::timeout(Duration::from_secs(5), h.handle.task)
        .await
        .expect("agent loop did not stop")
        .unwrap();
}

#[tokio::test]
async fn shutdown_during_a_turn_cancels_it_and_still_ends_the_turn() {
    let slow = ok(text_stream("too late")).set_delay(Duration::from_secs(30));
    let server = model_server(vec![slow]).await;
    let mut h = start(&server).await;
    let _clone = h.handle.commands.clone();

    h.handle.commands.send(AgentCommand::Prompt("hi".into())).unwrap();
    h.handle.commands.send(AgentCommand::Prompt("queued".into())).unwrap();
    loop {
        if h.handle.events.recv().await.unwrap() == AgentEvent::RequestStarted {
            break;
        }
    }
    h.handle.commands.send(AgentCommand::Shutdown).unwrap();

    let events = until_turn_ended(&mut h).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnded { reason: TurnEnd::Cancelled }));
    tokio::time::timeout(Duration::from_secs(5), h.handle.task)
        .await
        .expect("agent loop did not stop")
        .unwrap();
    // The queued prompt was dropped, not run: no second turn, and the
    // event channel closed with the loop.
    let mut rest = Vec::new();
    while let Some(ev) = h.handle.events.recv().await {
        rest.push(ev);
    }
    assert!(rest.is_empty(), "{rest:?}");
}

// ---------------------------------------------------------------------------
// Session log (Phase 1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn killing_mid_tool_loses_nothing_and_resume_answers_the_call() {
    let server = model_server(vec![ok(tool_use_stream("tu_1", "sleep 20; echo never"))]).await;
    let mut h = start(&server).await;
    let id = h.session.id.clone();

    h.send(AgentCommand::Prompt("do the slow thing".into()));
    loop {
        if let AgentEvent::ToolCallStarted { .. } = next_event(&mut h).await {
            break;
        }
    }
    // Kill the loop mid-tool, as a crash would.
    h.handle.task.abort();
    let _ = (&mut h.handle.task).await;

    // The prompt and the tool call are on disk.
    let msgs = History::from_entries(&h.log(&id)).messages;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0], Message::user_text("do the slow thing"));
    assert_eq!(msgs[1].role, "assistant");

    // Resume in a new agent (a new process, as far as the log can tell).
    let server2 = model_server(vec![ok(text_stream("picked up"))]).await;
    let mut h2 = start_with(test_config(&server2), h.tmp, SessionStart::Resume(id.clone())).await;
    assert_eq!(h2.session.id, id);
    assert!(h2.session.resumed);
    h2.send(AgentCommand::Prompt("carry on".into()));
    let events = until_turn_ended(&mut h2).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnded { reason: TurnEnd::Completed(StopReason::EndTurn) }));

    let body = &request_bodies(&server2).await[0];
    let sent = body["messages"].as_array().unwrap();
    assert_eq!(sent.len(), 4, "{sent:#?}");
    assert_eq!(sent[2]["content"][0]["type"], "tool_result");
    assert_eq!(sent[2]["content"][0]["tool_use_id"], "tu_1");
    assert_eq!(sent[2]["content"][0]["is_error"], true);
    assert_eq!(sent[3]["content"], "carry on");
    // The resumed request reuses the system prompt the session started with.
    let logged_system = h2.log(&id).iter().find_map(|e| match &e.kind {
        EntryKind::SystemPrompt { blocks } => Some(blocks[0].text.clone()),
        _ => None,
    });
    assert_eq!(body["system"][0]["text"].as_str(), logged_system.as_deref());
    // And the repair was logged, followed by the resume marker.
    let kinds: Vec<&str> = h2.log(&id).iter().map(kind_name).collect();
    assert!(kinds.ends_with(&["tool_result", "resumed", "message", "message"]), "{kinds:?}");
}

#[tokio::test]
async fn fork_continues_from_before_a_prompt() {
    let server = model_server(vec![
        ok(text_stream("first")),
        ok(text_stream("second")),
        ok(text_stream("alternative")),
    ])
    .await;
    let mut h = start(&server).await;
    let original = h.session.id.clone();
    h.send(AgentCommand::Prompt("one".into()));
    until_turn_ended(&mut h).await;
    h.send(AgentCommand::Prompt("two".into()));
    until_turn_ended(&mut h).await;

    let prompts = session::user_prompts(&h.log(&original));
    assert_eq!(prompts.len(), 2);
    h.send(AgentCommand::Fork { at: prompts[1].parent.clone() });
    let info = next_session(&mut h).await;
    assert_ne!(info.id, original);
    assert_eq!(info.forked_from.as_deref(), Some(original.as_str()));
    assert_eq!(info.message_count, 2);

    h.send(AgentCommand::Prompt("two, differently".into()));
    until_turn_ended(&mut h).await;
    let bodies = request_bodies(&server).await;
    let texts: Vec<Value> = bodies[2]["messages"].as_array().unwrap().iter().map(|m| m["content"].clone()).collect();
    assert_eq!(texts[0], "one");
    assert_eq!(texts[2], "two, differently");
    assert_eq!(texts.len(), 3);

    // The original is untouched and both are listed.
    assert_eq!(History::from_entries(&h.log(&original)).messages.len(), 4);
    let listed: Vec<String> = session::list_sessions(&h.sessions_dir()).into_iter().map(|s| s.id).collect();
    assert_eq!(listed, [info.id.clone(), original]);
}

#[tokio::test]
async fn new_and_load_session_switch_the_history() {
    let server = model_server(vec![ok(text_stream("a"))]).await;
    let mut h = start(&server).await;
    let first = h.session.id.clone();
    h.send(AgentCommand::Prompt("in the first".into()));
    until_turn_ended(&mut h).await;

    h.send(AgentCommand::NewSession);
    let second = next_session(&mut h).await;
    assert_ne!(second.id, first);
    assert_eq!(second.message_count, 0);
    assert!(h.state.messages.lock().unwrap().is_empty());
    h.send(AgentCommand::Prompt("in the second".into()));
    until_turn_ended(&mut h).await;

    h.send(AgentCommand::SetSessionName("side quest".into()));
    assert_eq!(next_session(&mut h).await.name.as_deref(), Some("side quest"));

    h.send(AgentCommand::LoadSession(first.clone()));
    let back = next_session(&mut h).await;
    assert_eq!((back.id.as_str(), back.message_count, back.resumed), (first.as_str(), 2, true));
    h.send(AgentCommand::Prompt("back again".into()));
    until_turn_ended(&mut h).await;

    let bodies = request_bodies(&server).await;
    assert_eq!(bodies[1]["messages"].as_array().unwrap().len(), 1);
    assert_eq!(bodies[2]["messages"].as_array().unwrap().len(), 3);
    assert_eq!(session::find_session(&h.sessions_dir(), "side quest").unwrap(), second.id);

    h.send(AgentCommand::LoadSession("no-such-session".into()));
    assert!(matches!(next_event(&mut h).await, AgentEvent::Error { .. }));
}

#[tokio::test]
async fn compaction_is_token_based_and_keeps_the_transcript() {
    let server = model_server(vec![
        ok(text_stream_using("first", 1_900)),
        ok(text_stream("SUMMARY OF ONE")),
        ok(text_stream("second")),
    ])
    .await;
    let config = AppConfig { context_window: Some(2_000), compact_keep_tokens: 50, ..test_config(&server) };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::Prompt("one".into()));
    until_turn_ended(&mut h).await;
    // 1900 + 3 tokens is within 15% of a 2000-token window.
    h.send(AgentCommand::Prompt("two".into()));
    let events = until_turn_ended(&mut h).await;
    assert!(events.contains(&AgentEvent::Compacted { before: 3, after: 3 }), "{events:?}");

    let bodies = request_bodies(&server).await;
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[1]["model"], "claude-haiku-5-5");
    let sent = bodies[2]["messages"].as_array().unwrap();
    assert!(sent[0]["content"].as_str().unwrap().contains("SUMMARY OF ONE"));
    assert_eq!(sent[2]["content"], "two");

    // The log still has every message; only the derived history is short.
    let log = h.log(&h.session.id);
    let all: Vec<&Message> = log
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Message { message } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(all.len(), 4);
    assert!(log.iter().any(|e| matches!(&e.kind, EntryKind::Compaction { summary: Some(s), .. } if s == "SUMMARY OF ONE")));
    assert_eq!(History::from_entries(&log).messages.len(), 4, "summary, ack, two, second");
}

#[tokio::test]
async fn context_overflow_compacts_and_retries() {
    let overflow = ResponseTemplate::new(400).set_body_string(
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
    );
    let server = model_server(vec![
        ok(text_stream("first")),
        overflow,
        ok(text_stream("SUMMARY")),
        ok(text_stream("fits now")),
    ])
    .await;
    let config = AppConfig { compact_keep_tokens: 10, ..test_config(&server) };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::Prompt("one".into()));
    until_turn_ended(&mut h).await;
    h.send(AgentCommand::Prompt("two".into()));
    let events = until_turn_ended(&mut h).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnded { reason: TurnEnd::Completed(StopReason::EndTurn) }));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Compacted { .. })), "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })), "{events:?}");
    let bodies = request_bodies(&server).await;
    assert_eq!(bodies.len(), 4);
    assert!(bodies[3]["messages"][0]["content"].as_str().unwrap().contains("SUMMARY"));
}

#[tokio::test]
async fn large_bash_output_is_spilled_to_a_file() {
    let server = model_server(vec![
        ok(tool_use_stream("tu_big", "seq 1 500")),
        ok(text_stream("read it")),
    ])
    .await;
    let mut h = start(&server).await;
    h.send(AgentCommand::Prompt("count".into()));
    let events = until_turn_ended(&mut h).await;
    let output = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    let id = &h.session.id;
    let handle = format!("/workspace/.syl/spill/{id}/tu_big.txt");
    assert!(output.contains(&handle), "{output}");
    assert!(output.contains("500 lines"), "{output}");
    assert!(output.contains("  100: 100") && !output.contains("101: 101"), "{output}");
    let saved = std::fs::read_to_string(h.project().join(format!(".syl/spill/{id}/tu_big.txt"))).unwrap();
    assert_eq!(saved.lines().count(), 500);
    // The model saw the preview, and the log holds exactly that.
    let msgs = h.state.messages.lock().unwrap().clone();
    let MessageContent::Blocks(blocks) = &msgs[2].content else { panic!() };
    assert!(matches!(&blocks[0], ContentBlock::ToolResult { content, .. } if *content == output));
}

#[tokio::test]
async fn overflow_with_nothing_to_compact_fails_instead_of_resending() {
    let overflow = || {
        ResponseTemplate::new(400).set_body_string(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
        )
    };
    let server = model_server(vec![overflow(), overflow()]).await;
    let mut h = start(&server).await;
    // One prompt: there is no older history to summarize.
    h.send(AgentCommand::Prompt("huge".into()));
    let events = until_turn_ended(&mut h).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnded { reason: TurnEnd::Failed }));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Error { message } if message.contains("too long"))));
    assert_eq!(request_bodies(&server).await.len(), 1, "the same request was not sent again");
}

#[tokio::test]
async fn summarizer_gets_no_tools_and_an_unfinished_summary_is_not_used() {
    let cut_off = sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":5,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half a summ"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":3}}),
    ]);
    let server = model_server(vec![
        ok(text_stream_using("first", 1_900)),
        ok(cut_off),
        ok(text_stream("second")),
    ])
    .await;
    let config = AppConfig { context_window: Some(2_000), compact_keep_tokens: 50, ..test_config(&server) };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::Prompt("one".into()));
    until_turn_ended(&mut h).await;
    h.send(AgentCommand::Prompt("two".into()));
    let events = until_turn_ended(&mut h).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Notice { message } if message.contains("Could not summarize"))));
    let bodies = request_bodies(&server).await;
    assert!(bodies[1].get("tools").is_none(), "the summarizer must answer in text");
    assert!(bodies[0].get("tools").is_some());
    let first = bodies[2]["messages"][0]["content"].to_string();
    assert!(!first.contains("half a summ"), "{first}");
}

#[tokio::test]
async fn set_model_behind_a_queued_prompt_waits_for_it() {
    let first = ok(text_stream("a")).set_delay(Duration::from_millis(300));
    let server = model_server(vec![first, ok(text_stream("b"))]).await;
    let mut h = start(&server).await;
    h.send(AgentCommand::Prompt("one".into()));
    loop {
        if next_event(&mut h).await == AgentEvent::RequestStarted {
            break;
        }
    }
    h.send(AgentCommand::Prompt("two".into()));
    h.send(AgentCommand::SetModel("claude-haiku-5-5".into()));
    until_turn_ended(&mut h).await;
    until_turn_ended(&mut h).await;
    let bodies = request_bodies(&server).await;
    assert_ne!(bodies[1]["model"], "claude-haiku-5-5", "prompt two was sent before the model change");
}
