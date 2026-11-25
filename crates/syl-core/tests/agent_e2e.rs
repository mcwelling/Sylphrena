//! End-to-end tests of the agent loop with no terminal: a wiremock model and
//! a stand-in podman, asserting on the event stream.
mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use syl_core::agent::{self, Agent, AgentCommand, AgentEvent, AgentHandle, Mode, SessionPaths, SessionStart, TurnEnd};
use syl_core::control::PermissionAnswer;
use syl_core::permissions::{Action, Rule};
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
    // A run_bash call is checkpointed first; the commit varies.
    let (checkpoints, events): (Vec<AgentEvent>, Vec<AgentEvent>) =
        events.into_iter().partition(|e| matches!(e, AgentEvent::Checkpointed { .. }));
    assert_eq!(checkpoints.len(), 1);
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
        ["session", "message", "model_change", "system_prompt", "message", "checkpoint", "tool_result", "message"]
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
        EntryKind::Steer { .. } => "steer",
        EntryKind::ImagesDropped { .. } => "images_dropped",
        EntryKind::Checkpoint { .. } => "checkpoint",
        EntryKind::Restored { .. } => "restored",
        EntryKind::Note { .. } => "note",
        EntryKind::ToolSet { .. } => "tool_set",
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

// ---------------------------------------------------------------------------
// Tool parity (Phase 2)
// ---------------------------------------------------------------------------

/// One assistant turn making these tool calls, in order.
fn tool_calls_stream(calls: &[(&str, &str, Value)]) -> String {
    let mut events = vec![json!({"type":"message_start","message":{"usage":{"input_tokens":20,"output_tokens":0}}})];
    for (i, (id, name, input)) in calls.iter().enumerate() {
        events.push(json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":id,"name":name}}));
        events.push(json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
        events.push(json!({"type":"content_block_stop","index":i}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}));
    sse(&events)
}

#[tokio::test]
async fn edit_file_changes_the_project_through_the_container() {
    let server = model_server(vec![
        ok(tool_calls_stream(&[(
            "e1",
            "edit_file",
            json!({"path": "src/main.rs", "old_string": "println!(\"hi\")", "new_string": "println!(\"hello\")"}),
        )])),
        ok(text_stream("done")),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("project/src")).unwrap();
    std::fs::write(tmp.path().join("project/src/main.rs"), "fn main() { println!(\"hi\") }\n").unwrap();
    let mut h = start_with(test_config(&server), tmp, SessionStart::New).await;
    h.send(AgentCommand::Prompt("say hello".into()));
    let events = until_turn_ended(&mut h).await;
    let out = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished { output, is_error: false, .. } => Some(output.clone()),
            _ => None,
        })
        .expect("edit succeeded");
    assert!(out.contains("+fn main() { println!(\"hello\") }"), "{out}");
    assert_eq!(
        std::fs::read_to_string(h.project().join("src/main.rs")).unwrap(),
        "fn main() { println!(\"hello\") }\n"
    );
}

#[tokio::test]
async fn read_only_calls_run_together_and_others_one_at_a_time() {
    let server = model_server(vec![
        ok(tool_calls_stream(&[
            ("r1", "ls", json!({})),
            ("r2", "grep", json!({"pattern": "x"})),
            ("w1", "write_file", json!({"path": "a.txt", "content": "x\n"})),
            ("r3", "read_file", json!({"path": "a.txt"})),
        ])),
        ok(text_stream("done")),
    ])
    .await;
    let mut h = start(&server).await;
    h.send(AgentCommand::Prompt("go".into()));
    let order: Vec<String> = until_turn_ended(&mut h)
        .await
        .into_iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallStarted { id, .. } => Some(format!("start {id}")),
            AgentEvent::ToolCallFinished { id, .. } => Some(format!("end {id}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        ["start r1", "start r2", "end r1", "end r2", "start w1", "end w1", "start r3", "end r3"]
    );
    // The read after the write saw it, and all four results went back together.
    let bodies = request_bodies(&server).await;
    let results = bodies[1]["messages"].as_array().unwrap().last().unwrap()["content"].as_array().unwrap().clone();
    let ids: Vec<&str> = results.iter().map(|r| r["tool_use_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["r1", "r2", "w1", "r3"]);
    assert!(results[3]["content"].as_str().unwrap().contains("1: x"));
}

#[tokio::test]
async fn steering_is_delivered_with_the_tool_results() {
    let server = model_server(vec![
        ok(tool_use_stream("slow", "sleep 1; echo working")),
        ok(text_stream("adjusted")),
    ])
    .await;
    let mut h = start(&server).await;
    h.send(AgentCommand::Prompt("start".into()));
    loop {
        if let AgentEvent::ToolCallStarted { .. } = next_event(&mut h).await {
            break;
        }
    }
    h.send(AgentCommand::Steer("use the other approach".into()));
    let events = until_turn_ended(&mut h).await;
    assert!(events.contains(&AgentEvent::SteerDelivered { text: "use the other approach".into() }), "{events:?}");

    let bodies = request_bodies(&server).await;
    assert_eq!(bodies.len(), 2, "steering doesn't start another turn");
    let last = bodies[1]["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["content"][0]["type"], "tool_result");
    assert_eq!(last["content"][1], json!({"type": "text", "text": "use the other approach"}));
    // It is in the log, and survives a reload.
    let msgs = History::from_entries(&h.log(&h.session.id)).messages;
    assert_eq!(serde_json::to_value(&msgs[2]).unwrap(), last);
}

#[tokio::test]
async fn steering_after_the_last_tool_call_becomes_the_next_prompt() {
    let slow_text = ok(text_stream("finished")).set_delay(Duration::from_millis(300));
    let server = model_server(vec![slow_text, ok(text_stream("about that"))]).await;
    let mut h = start(&server).await;
    h.send(AgentCommand::Prompt("one".into()));
    loop {
        if next_event(&mut h).await == AgentEvent::RequestStarted {
            break;
        }
    }
    h.send(AgentCommand::Steer("and another thing".into()));
    until_turn_ended(&mut h).await;
    until_turn_ended(&mut h).await;
    let bodies = request_bodies(&server).await;
    assert_eq!(bodies[1]["messages"].as_array().unwrap().last().unwrap()["content"], "and another thing");
}

#[tokio::test]
async fn agents_md_is_in_the_system_prompt() {
    let server = model_server(vec![ok(text_stream("ok"))]).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("project")).unwrap();
    std::fs::write(tmp.path().join("CLAUDE.md"), "Outer rule: be brief.").unwrap();
    std::fs::write(tmp.path().join("project/AGENTS.md"), "Run `make check` before finishing.").unwrap();
    std::fs::write(tmp.path().join("project/CLAUDE.md"), "IGNORED when AGENTS.md exists").unwrap();
    let mut h = start_with(test_config(&server), tmp, SessionStart::New).await;
    h.send(AgentCommand::Prompt("hi".into()));
    until_turn_ended(&mut h).await;
    let system = request_bodies(&server).await[0]["system"][0]["text"].as_str().unwrap().to_string();
    let outer = system.find("Outer rule: be brief.").expect("parent CLAUDE.md");
    let inner = system.find("Run `make check` before finishing.").expect("project AGENTS.md");
    assert!(outer < inner, "the project's own file comes last");
    assert!(!system.contains("IGNORED"));
}

#[tokio::test]
async fn image_prompts_send_image_blocks() {
    use syl_core::api::types::ImageSource;
    let server = model_server(vec![ok(text_stream("a cat"))]).await;
    let mut h = start(&server).await;
    h.send(AgentCommand::PromptWithImages {
        text: "what is in [image: cat.png]?".into(),
        images: vec![ImageSource::base64("image/png", "iVBORw0KGgo=")],
    });
    until_turn_ended(&mut h).await;
    let content = request_bodies(&server).await[0]["messages"][0]["content"].clone();
    assert_eq!(content[0]["type"], "image");
    assert_eq!(content[0]["source"], json!({"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}));
    assert_eq!(content[1]["text"], "what is in [image: cat.png]?");
    let prompts = session::user_prompts(&h.log(&h.session.id));
    assert_eq!(prompts[0].text, "what is in [image: cat.png]?");
}

#[tokio::test]
async fn a_request_rejected_over_images_is_retried_without_them() {
    use syl_core::api::types::ImageSource;
    let too_big = ResponseTemplate::new(413).set_body_string(
        r#"{"type":"error","error":{"type":"request_too_large","message":"Request exceeds the maximum allowed number of bytes."}}"#,
    );
    let server = model_server(vec![too_big, ok(text_stream("ok without it"))]).await;
    let mut h = start(&server).await;
    h.send(AgentCommand::PromptWithImages {
        text: "look".into(),
        images: vec![ImageSource::base64("image/png", "AAAA")],
    });
    let events = until_turn_ended(&mut h).await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnded { reason: TurnEnd::Completed(StopReason::EndTurn) }));
    let bodies = request_bodies(&server).await;
    assert_eq!(bodies[1]["messages"][0]["content"][0]["type"], "text", "the image was taken out");
    // ... and stays out: it is in the log.
    let log = h.log(&h.session.id);
    assert!(log.iter().any(|e| matches!(e.kind, EntryKind::ImagesDropped { all: true })));
}

#[tokio::test]
async fn late_steering_is_announced_then_run_as_the_next_prompt() {
    let slow_text = ok(text_stream("finished")).set_delay(Duration::from_millis(300));
    let server = model_server(vec![slow_text, ok(text_stream("about that"))]).await;
    let mut h = start(&server).await;
    h.send(AgentCommand::Prompt("one".into()));
    loop {
        if next_event(&mut h).await == AgentEvent::RequestStarted {
            break;
        }
    }
    h.send(AgentCommand::Steer("late".into()));
    let first = until_turn_ended(&mut h).await;
    assert!(matches!(first.last(), Some(AgentEvent::TurnEnded { .. })));
    // Right after TurnEnded: the queued notice, then the new turn.
    assert_eq!(next_event(&mut h).await, AgentEvent::SteerQueued { text: "late".into(), count: 1 });
    assert_eq!(next_event(&mut h).await, AgentEvent::TurnStarted);
    until_turn_ended(&mut h).await;
}

#[tokio::test]
async fn steering_when_idle_is_a_prompt_and_says_so() {
    let server = model_server(vec![ok(text_stream("hi"))]).await;
    let mut h = start(&server).await;
    h.send(AgentCommand::Steer("hello".into()));
    assert_eq!(next_event(&mut h).await, AgentEvent::SteerQueued { text: "hello".into(), count: 1 });
    assert_eq!(next_event(&mut h).await, AgentEvent::TurnStarted);
    until_turn_ended(&mut h).await;
}

#[tokio::test]
async fn dispatch_agent_gets_the_full_page_cut_on_a_character_boundary() {
    // A long plain-text page of 3-byte characters: over 100 lines (which
    // run_bash would have turned into a numbered preview) and longer than
    // the 32,000-byte cap, which lands inside a character.
    let page: String = (0..2_000).map(|i| format!("日本語の行です {i:04}\n")).collect();
    assert!(!page.is_char_boundary(32_000), "the cap must split a character for this test");
    let site = MockServer::start().await;
    Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_string(page.clone())).mount(&site).await;

    let server = model_server(vec![
        ok(tool_calls_stream(&[(
            "d1",
            "dispatch_agent",
            json!({"task": "fetch", "source": format!("{}/doc.txt", site.uri()), "focus": "line numbers",
                   "output_format": "text", "max_length": 300}),
        )])),
        ok(text_stream("sub-agent answer")),
        ok(text_stream("done")),
    ])
    .await;
    let mut h = start(&server).await;
    h.send(AgentCommand::Prompt("fetch it".into()));
    let events = until_turn_ended(&mut h).await;
    let out = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished { output, .. } => Some(output.clone()),
            _ => None,
        })
        .unwrap();
    assert!(out.contains("sub-agent answer"), "{out}");

    let bodies = request_bodies(&server).await;
    let sub = &bodies[1];
    assert!(sub.get("tools").is_none(), "the sub-agent is offered no tools");
    let prompt = sub["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("日本語の行です 0000") && prompt.contains("日本語の行です 1100"), "the page is not cut to 100 lines");
    assert!(!prompt.contains("read_file") && !prompt.contains("    1: "), "no run_bash preview formatting");
    assert!(prompt.contains("of 54000 bytes shown"), "the cut is reported");
}

// ---------------------------------------------------------------------------
// Control and safety (Phase 3)
// ---------------------------------------------------------------------------

fn ask_rule(pattern: &str) -> Rule {
    Rule { tool: "run_bash".into(), pattern: pattern.into(), action: Action::Ask }
}

/// Wait for a PermissionRequested, collecting what came before.
async fn until_asked(h: &mut Harness) -> (String, String) {
    loop {
        match next_event(h).await {
            AgentEvent::PermissionRequested { id, subject, .. } => return (id, subject),
            AgentEvent::TurnEnded { .. } => panic!("the turn ended without asking"),
            _ => {}
        }
    }
}

fn result_of(events: &[AgentEvent], id: &str) -> (String, bool) {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolCallFinished { id: i, output, is_error, .. } if i == id => Some((output.clone(), *is_error)),
            _ => None,
        })
        .expect("the call finished")
}

#[tokio::test]
async fn an_ask_rule_waits_for_the_user_and_runs_once_approved() {
    let server = model_server(vec![ok(tool_use_stream("t1", "true && echo pushed")), ok(text_stream("done"))]).await;
    let config = AppConfig { permissions: vec![ask_rule("echo *")], ..test_config(&server) };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::Prompt("go".into()));
    let (id, subject) = until_asked(&mut h).await;
    assert_eq!(subject, "echo pushed", "only the part that needs asking is shown");
    h.send(AgentCommand::ApprovePermission { id: id.clone(), answer: PermissionAnswer::Once });
    let events = until_turn_ended(&mut h).await;
    assert!(result_of(&events, &id).0.contains("pushed"));
    assert!(h.log(&h.session.id).iter().any(|e| matches!(&e.kind,
        EntryKind::Approval { decision, by, .. } if decision == "allow" && by == "user")));
}

#[tokio::test]
async fn a_rejected_call_tells_the_model_and_a_deny_rule_never_asks() {
    let server = model_server(vec![
        ok(tool_calls_stream(&[
            ("a", "run_bash", json!({"command": "echo one"})),
            ("b", "run_bash", json!({"command": "rm -rf build"})),
        ])),
        ok(text_stream("ok")),
    ])
    .await;
    let config = AppConfig {
        permissions: vec![ask_rule("echo *"), Rule { tool: "*".into(), pattern: "rm -rf *".into(), action: Action::Deny }],
        ..test_config(&server)
    };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::Prompt("go".into()));
    let (id, _) = until_asked(&mut h).await;
    h.send(AgentCommand::ApprovePermission { id, answer: PermissionAnswer::Reject });
    let events = until_turn_ended(&mut h).await;
    let (a, a_err) = result_of(&events, "a");
    assert!(a_err && a.contains("declined"), "{a}");
    let (b, b_err) = result_of(&events, "b");
    assert!(b_err && b.contains("denied") && b.contains("rm -rf build"), "{b}");
    let asks = events.iter().filter(|e| matches!(e, AgentEvent::PermissionRequested { .. })).count();
    assert_eq!(asks, 0, "only the first call asked (before these events)");
}

#[tokio::test]
async fn always_saves_a_rule_so_the_same_command_isnt_asked_again() {
    let server = model_server(vec![
        ok(tool_use_stream("t1", "echo again")),
        ok(tool_use_stream("t2", "echo again")),
        ok(text_stream("done")),
    ])
    .await;
    let config = AppConfig { permissions: vec![ask_rule("echo *")], ..test_config(&server) };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::Prompt("go".into()));
    let (id, _) = until_asked(&mut h).await;
    h.send(AgentCommand::ApprovePermission { id, answer: PermissionAnswer::Always });
    let events = until_turn_ended(&mut h).await;
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::PermissionRequested { .. })), "not asked twice");
    assert!(!result_of(&events, "t2").1);
    let saved = std::fs::read_to_string(h.project().join(".syl/approvals.toml")).unwrap();
    assert!(saved.contains("echo again") && saved.contains("allow"), "{saved}");
}

#[tokio::test]
async fn the_reviewer_can_approve_and_fails_closed() {
    use syl_core::config::ReviewerConfig;
    // First call: the reviewer allows. Second: the reviewer's request fails,
    // so the user is asked.
    let server = model_server(vec![
        ok(tool_use_stream("t1", "echo one")),
        ok(text_stream("ALLOW: a harmless echo")),
        ok(tool_use_stream("t2", "echo two")),
        ResponseTemplate::new(400).set_body_string("reviewer broke"),
        ok(text_stream("done")),
    ])
    .await;
    let config = AppConfig {
        permissions: vec![ask_rule("echo *")],
        reviewer: Some(ReviewerConfig { model: "claude-haiku-4-5".into(), timeout_secs: 5 }),
        ..test_config(&server)
    };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::Prompt("go".into()));
    let (id, _) = until_asked(&mut h).await;
    assert_eq!(id, "t2", "the reviewer approved t1 without asking");
    h.send(AgentCommand::ApprovePermission { id, answer: PermissionAnswer::Once });
    until_turn_ended(&mut h).await;
    let bodies = request_bodies(&server).await;
    assert_eq!(bodies[1]["model"], "claude-haiku-4-5");
    assert!(bodies[1].get("tools").is_none());
    assert!(bodies[1]["messages"][0]["content"].as_str().unwrap().contains("echo one"));
    let log = h.log(&h.session.id);
    assert!(log.iter().any(|e| matches!(&e.kind, EntryKind::Approval { by, .. } if by == "reviewer")));
}

#[tokio::test]
async fn plan_mode_refuses_edits_and_puts_back_what_run_bash_changes() {
    let server = model_server(vec![
        ok(tool_calls_stream(&[
            ("e", "write_file", json!({"path": "plan.md", "content": "x"})),
            ("b", "run_bash", json!({"command": "echo made > made.txt; rm keep.txt; echo looked"})),
        ])),
        ok(text_stream("here is my plan")),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("project")).unwrap();
    std::fs::write(tmp.path().join("project/keep.txt"), "keep me\n").unwrap();
    let mut h = start_with(test_config(&server), tmp, SessionStart::New).await;
    h.send(AgentCommand::SetMode(Mode::Plan));
    assert_eq!(next_event(&mut h).await, AgentEvent::ModeChanged { mode: Mode::Plan });
    h.send(AgentCommand::Prompt("plan it".into()));
    let events = until_turn_ended(&mut h).await;

    let (e, e_err) = result_of(&events, "e");
    assert!(e_err && e.contains("Plan mode"), "{e}");
    let (b, b_err) = result_of(&events, "b");
    assert!(b_err && b.contains("looked") && b.contains("put back"), "{b}");
    assert!(!h.project().join("plan.md").exists());
    assert!(!h.project().join("made.txt").exists());
    assert_eq!(std::fs::read_to_string(h.project().join("keep.txt")).unwrap(), "keep me\n");
    // The model was told it is in plan mode.
    let first = request_bodies(&server).await[0]["messages"][0]["content"].to_string();
    assert!(first.contains("plan mode"), "{first}");
}

#[tokio::test]
async fn a_turn_is_checkpointed_and_can_be_restored() {
    let server = model_server(vec![
        ok(tool_use_stream("t1", "rm -rf src")),
        ok(text_stream("deleted it")),
        ok(text_stream("noted")),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("project/src")).unwrap();
    std::fs::write(tmp.path().join("project/src/main.rs"), "fn main() {}\n").unwrap();
    let mut h = start_with(test_config(&server), tmp, SessionStart::New).await;
    h.send(AgentCommand::Prompt("clean up".into()));
    let events = until_turn_ended(&mut h).await;
    let commit = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Checkpointed { commit } => Some(commit.clone()),
            _ => None,
        })
        .expect("a checkpoint before rm");
    assert!(!h.project().join("src").exists());

    h.send(AgentCommand::Restore { commit: commit.clone() });
    loop {
        match next_event(&mut h).await {
            AgentEvent::Restored { commit: c, files } => {
                assert_eq!((c.as_str(), files), (commit.as_str(), 1));
                break;
            }
            AgentEvent::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
    assert_eq!(std::fs::read_to_string(h.project().join("src/main.rs")).unwrap(), "fn main() {}\n");

    // The model hears about it with the next prompt, and receipts show the turn.
    h.send(AgentCommand::Prompt("what happened?".into()));
    until_turn_ended(&mut h).await;
    let sent = request_bodies(&server).await[2]["messages"].to_string();
    assert!(sent.contains("restored the workspace"), "{sent}");
    let receipts = session::receipts(&h.log(&h.session.id));
    assert_eq!(receipts[0].commands, ["rm -rf src"]);
    assert_eq!(receipts[0].checkpoint.as_deref(), Some(commit.as_str()));
}

// ---------------------------------------------------------------------------
// Extensibility (Phase 5)
// ---------------------------------------------------------------------------

/// A tiny MCP server (stdio, NDJSON) written in Python: one read-only tool
/// `shout` and one `delete_all` that isn't, plus server instructions.
const MCP_SERVER: &str = r#"
import json, sys
for line in sys.stdin:
    msg = json.loads(line)
    mid, method = msg.get("id"), msg.get("method")
    if mid is None:
        continue
    if method == "initialize":
        result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                  "serverInfo": {"name": "toy", "version": "1"}, "instructions": "Use shout for emphasis."}
    elif method == "tools/list":
        result = {"tools": [
            {"name": "shout", "description": "Uppercase text", "annotations": {"readOnlyHint": True},
             "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}},
            {"name": "delete_all", "description": "Deletes things",
             "inputSchema": {"type": "object", "properties": {}}}]}
    elif method == "tools/call":
        args = msg["params"].get("arguments", {})
        if msg["params"]["name"] == "shout":
            result = {"content": [{"type": "text", "text": args.get("text", "").upper()}]}
        else:
            result = {"content": [{"type": "text", "text": "deleted"}], "isError": False}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "no"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}), flush=True)
"#;

fn mcp_config(server: &MockServer, tmp: &tempfile::TempDir) -> AppConfig {
    let script = tmp.path().join("project/toy_mcp.py");
    std::fs::create_dir_all(tmp.path().join("project")).unwrap();
    std::fs::write(&script, MCP_SERVER).unwrap();
    let mcp: syl_core::mcp::McpServerConfig =
        toml::from_str(&format!("name = \"toy\"\ncommand = \"python3\"\nargs = [\"{}\"]", script.display())).unwrap();
    AppConfig { mcp: vec![mcp], ..test_config(server) }
}

#[tokio::test]
async fn mcp_tools_are_offered_and_called_through_the_container() {
    let server = model_server(vec![
        ok(tool_calls_stream(&[("m1", "toy__shout", json!({"text": "hello"}))])),
        ok(text_stream("done")),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = mcp_config(&server, &tmp);
    let mut h = start_with(config, tmp, SessionStart::New).await;
    h.send(AgentCommand::Prompt("shout hello".into()));
    let events = until_turn_ended(&mut h).await;
    assert_eq!(result_of(&events, "m1"), ("HELLO".to_string(), false));
    let first = &request_bodies(&server).await[0];
    let names: Vec<&str> = first["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"toy__shout") && names.contains(&"toy__delete_all") && names.contains(&"run_bash"), "{names:?}");
    assert!(first["system"][0]["text"].as_str().unwrap().contains("Use shout for emphasis."));
    // The tool set is logged with the conversation.
    assert!(h.log(&h.session.id).iter().any(|e| matches!(e.kind, EntryKind::ToolSet { .. })));
}

#[tokio::test]
async fn plan_mode_refuses_mcp_tools_that_dont_say_they_only_read() {
    let server = model_server(vec![
        ok(tool_calls_stream(&[
            ("r", "toy__shout", json!({"text": "fine"})),
            ("w", "toy__delete_all", json!({})),
        ])),
        ok(text_stream("planned")),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let config = mcp_config(&server, &tmp);
    let mut h = start_with(config, tmp, SessionStart::New).await;
    h.send(AgentCommand::SetMode(Mode::Plan));
    next_event(&mut h).await;
    h.send(AgentCommand::Prompt("go".into()));
    let events = until_turn_ended(&mut h).await;
    assert_eq!(result_of(&events, "r").0, "FINE");
    let (w, w_err) = result_of(&events, "w");
    assert!(w_err && w.contains("denied"), "{w}");
}

#[tokio::test]
async fn a_failing_pre_tool_hook_blocks_the_call() {
    let server = model_server(vec![ok(tool_use_stream("t1", "echo ran > ran.txt")), ok(text_stream("ok"))]).await;
    let hook: syl_core::hooks::Hook = toml::from_str(
        "event = \"pre_tool\"\ntool = \"run_bash\"\ncommand = \"grep -q ran.txt && { echo 'no writing ran.txt' >&2; exit 1; } || exit 0\"",
    )
    .unwrap();
    let after: syl_core::hooks::Hook = toml::from_str("event = \"turn_end\"\ncommand = \"cat > turn_end.json\"").unwrap();
    let config = AppConfig { hooks: vec![hook, after], ..test_config(&server) };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::Prompt("go".into()));
    let events = until_turn_ended(&mut h).await;
    let (out, is_err) = result_of(&events, "t1");
    assert!(is_err && out.contains("no writing ran.txt"), "{out}");
    assert!(!h.project().join("ran.txt").exists());
    // The turn_end hook got the event as JSON on stdin.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let payload: Value = serde_json::from_str(&std::fs::read_to_string(h.project().join("turn_end.json")).unwrap()).unwrap();
    assert_eq!(payload["event"], "turn_end");
    assert_eq!(payload["reason"], "end_turn");
}

#[tokio::test]
async fn custom_commands_and_skills_expand_and_the_skill_tool_loads_one() {
    let server = model_server(vec![
        ok(tool_calls_stream(&[("s1", "skill", json!({"name": "release"}))])),
        ok(text_stream("ok")),
    ])
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    std::fs::create_dir_all(project.join(".syl/commands")).unwrap();
    std::fs::create_dir_all(project.join(".syl/skills/release")).unwrap();
    std::fs::write(project.join(".syl/commands/review.md"), "Review the diff, focusing on $ARGUMENTS.").unwrap();
    std::fs::write(
        project.join(".syl/skills/release/SKILL.md"),
        "---\nname: release\ndescription: How to cut a release\n---\nBump the version, tag, push.",
    )
    .unwrap();
    let mut h = start_with(test_config(&server), tmp, SessionStart::New).await;
    h.send(AgentCommand::Prompt("/review error handling".into()));
    let events = until_turn_ended(&mut h).await;
    assert!(result_of(&events, "s1").0.contains("Bump the version, tag, push."));
    let first = &request_bodies(&server).await[0];
    assert_eq!(first["messages"][0]["content"], "Review the diff, focusing on error handling.");
    assert!(first["system"][0]["text"].as_str().unwrap().contains("- release: How to cut a release"));
    assert!(first["tools"].as_array().unwrap().iter().any(|t| t["name"] == "skill"));
}

#[tokio::test]
async fn in_plan_mode_what_tool_hooks_write_is_put_back_too() {
    let server = model_server(vec![ok(tool_use_stream("t1", "echo looked")), ok(text_stream("plan"))]).await;
    let pre: syl_core::hooks::Hook = toml::from_str("event = \"pre_tool\"\ncommand = \"echo pre > pre.txt\"").unwrap();
    let post: syl_core::hooks::Hook = toml::from_str("event = \"post_tool\"\ncommand = \"echo post >> post.txt\"").unwrap();
    let config = AppConfig { hooks: vec![pre, post], ..test_config(&server) };
    let mut h = start_with(config, tempfile::tempdir().unwrap(), SessionStart::New).await;
    h.send(AgentCommand::SetMode(Mode::Plan));
    assert_eq!(next_event(&mut h).await, AgentEvent::ModeChanged { mode: Mode::Plan });
    h.send(AgentCommand::Prompt("look".into()));
    let events = until_turn_ended(&mut h).await;
    let (out, is_err) = result_of(&events, "t1");
    assert!(is_err && out.contains("looked") && out.contains("put back"), "{out}");
    assert!(!h.project().join("pre.txt").exists());
    assert!(!h.project().join("post.txt").exists());
}

#[tokio::test]
async fn cancel_stops_a_slow_session_start_hook_and_mcp_startup() {
    let server = model_server(vec![ok(text_stream("hi"))]).await;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("project")).unwrap();
    let slow: syl_core::hooks::Hook = toml::from_str("event = \"session_start\"\ncommand = \"sleep 60\"\ntimeout_secs = 120").unwrap();
    // A server that never answers initialize.
    let hung: syl_core::mcp::McpServerConfig = toml::from_str("name = \"hung\"\ncommand = \"sleep\"\nargs = [\"600\"]").unwrap();
    let config = AppConfig { hooks: vec![slow], mcp: vec![hung], ..test_config(&server) };
    let mut h = start_with(config, tmp, SessionStart::New).await;
    let started = std::time::Instant::now();
    h.send(AgentCommand::Cancel);
    h.send(AgentCommand::Prompt("hello".into()));
    // The hook was stopped; the turn then waits on the MCP server.
    loop {
        if let AgentEvent::Notice { message } = next_event(&mut h).await {
            if message.contains("Starting 1 MCP") {
                break;
            }
        }
    }
    h.send(AgentCommand::Cancel);
    let events = until_turn_ended(&mut h).await;
    assert!(matches!(events.last(), Some(AgentEvent::TurnEnded { reason: TurnEnd::Cancelled })), "{events:?}");
    assert!(started.elapsed() < Duration::from_secs(20), "took {:?}", started.elapsed());
}
