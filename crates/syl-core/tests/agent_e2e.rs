//! End-to-end tests of the agent loop with no terminal: a wiremock model and
//! a stand-in podman, asserting on the event stream.
mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use syl_core::agent::{self, Agent, AgentCommand, AgentEvent, AgentHandle, SessionPaths, TurnEnd};
use syl_core::api::types::{StopReason, Usage};
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
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":30,"output_tokens":0}}}),
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
}

fn start(server: &MockServer) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();

    let config = AppConfig {
        api_url: format!("{}/v1/messages", server.uri()),
        api_key: "test-key".into(),
        timeout_ms: 10_000,
        ..Default::default()
    };

    let state = SharedState::new();
    *state.current_model.lock().unwrap() = config.model.clone();
    let model = Arc::new(AnthropicAdapter::new(reqwest::Client::new(), state.clone()));
    let handle = agent::spawn(Agent {
        config,
        state: state.clone(),
        model,
        container: Arc::new(common::fake_container(&project, &bin)),
        paths: SessionPaths::for_project(&project),
    });
    Harness { handle, state, tmp }
}

/// Collect events until the next `TurnEnded` (inclusive).
async fn until_turn_ended(h: &mut Harness) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(10), h.handle.events.recv())
            .await
            .expect("timed out waiting for an event")
            .expect("agent stopped");
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
    let mut h = start(&server);

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
    // and it was saved.
    assert_eq!(h.state.messages.lock().unwrap().len(), 4);
    let saved = h.tmp.path().join("project/.syl/session.json");
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(saved).unwrap()).unwrap();
    assert_eq!(saved.as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn cancel_stops_a_turn_in_flight() {
    let slow = ok(text_stream("too late")).set_delay(Duration::from_secs(30));
    let server = model_server(vec![slow]).await;
    let mut h = start(&server);

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
    let mut h = start(&server);

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
    let mut h = start(&server);

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
    let mut h = start(&server);

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
