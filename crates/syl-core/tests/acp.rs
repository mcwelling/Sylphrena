//! Syl as an ACP agent, driven by a scripted client over an in-memory pipe,
//! with a wiremock model and a stand-in podman.
mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use syl_core::acp::{serve, AcpOptions};
use syl_core::config::AppConfig;
use syl_core::permissions::{Action, Rule};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e)).collect()
}

fn text_stream(text: &str) -> String {
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
    ])
}

fn bash_stream(id: &str, command: &str) -> String {
    let input = json!({ "command": command }).to_string();
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":"run_bash"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
    ])
}

fn ok(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

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

struct Client {
    tmp: tempfile::TempDir,
    served: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    write: WriteHalf<tokio::io::DuplexStream>,
    lines: Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
    next_id: u64,
    /// Responses that arrived before we asked for them: JSON-RPC lets
    /// responses come in any order.
    early: Vec<Value>,
}

impl Client {
    fn project(&self) -> String {
        self.tmp.path().join("project").canonicalize().unwrap().to_string_lossy().into_owned()
    }

    async fn send(&mut self, msg: Value) {
        let mut line = msg.to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(10), self.lines.next_line())
            .await
            .expect("timed out waiting for the agent")
            .unwrap()
            .expect("the agent closed the connection");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
    }

    async fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await;
        id
    }

    /// Collect messages until the response to `id`.
    async fn until_response(&mut self, id: u64) -> (Vec<Value>, Value) {
        if let Some(i) = self.early.iter().position(|m| m["id"] == json!(id)) {
            return (Vec::new(), self.early.remove(i));
        }
        let mut seen = Vec::new();
        loop {
            let msg = self.recv().await;
            let is_response = msg.get("method").is_none() && msg.get("id").is_some_and(|i| !i.is_null());
            if is_response && msg["id"] == json!(id) {
                return (seen, msg);
            }
            if is_response {
                self.early.push(msg);
            } else {
                seen.push(msg);
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> (Vec<Value>, Value) {
        let id = self.request(method, params).await;
        self.until_response(id).await
    }

    async fn new_session(&mut self) -> String {
        self.call("initialize", json!({"protocolVersion": 1, "clientCapabilities": {}})).await;
        let cwd = self.project();
        let (_, r) = self.call("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
        r["result"]["sessionId"].as_str().expect("a session id").to_string()
    }
}

fn start(server: &MockServer, permissions: Vec<Rule>) -> Client {
    start_in(server, permissions, tempfile::tempdir().unwrap())
}

fn start_in(server: &MockServer, permissions: Vec<Rule>, tmp: tempfile::TempDir) -> Client {
    let project = tmp.path().join("project");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let api_url = format!("{}/v1/messages", server.uri());
    let bin2 = bin.clone();
    let opts = AcpOptions {
        container: Arc::new(move |dir: &Path, _: &AppConfig| common::fake_container(dir, &bin2)),
        config: Arc::new(move |_: &Path| AppConfig {
            api_url: api_url.clone(),
            api_key: "test".into(),
            timeout_ms: 10_000,
            permissions: permissions.clone(),
            ..Default::default()
        }),
        http: reqwest::Client::new(),
        start_containers: false,
    };
    let (client_io, agent_io) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(agent_io);
    let served = tokio::spawn(async move { serve(ar, aw, opts).await });
    let (cr, cw) = tokio::io::split(client_io);
    Client { tmp, served: Some(served), write: cw, lines: BufReader::new(cr).lines(), next_id: 0, early: Vec::new() }
}

fn updates(msgs: &[Value]) -> Vec<Value> {
    msgs.iter().filter(|m| m["method"] == "session/update").map(|m| m["params"]["update"].clone()).collect()
}

#[tokio::test]
async fn initialize_comes_first_and_says_what_syl_supports() {
    let server = model_server(vec![ok(text_stream("hi"))]).await;
    let mut c = start(&server, vec![]);
    let (_, early) = c.call("session/new", json!({"cwd": "/", "mcpServers": []})).await;
    assert_eq!(early["error"]["code"], -32600, "{early}");
    let (_, r) = c.call("initialize", json!({"protocolVersion": 1})).await;
    let r = &r["result"];
    assert_eq!(r["protocolVersion"], 1);
    assert_eq!(r["agentCapabilities"]["loadSession"], true);
    assert_eq!(r["agentCapabilities"]["promptCapabilities"]["image"], true);
    assert_eq!(r["agentInfo"]["name"], "syl");
    let (_, unknown) = c.call("session/fly", json!({})).await;
    assert_eq!(unknown["error"]["code"], -32601);
    let (_, relative) = c.call("session/new", json!({"cwd": "project", "mcpServers": []})).await;
    assert_eq!(relative["error"]["code"], -32602);
}

#[tokio::test]
async fn a_prompt_streams_tool_calls_and_text_then_ends_the_turn() {
    let server = model_server(vec![ok(bash_stream("t1", "echo from-tool")), ok(text_stream("All done."))]).await;
    let mut c = start(&server, vec![]);
    let sid = c.new_session().await;
    let (msgs, r) = c.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]})).await;
    assert_eq!(r["result"]["stopReason"], "end_turn", "{r}");
    let u = updates(&msgs);
    let kinds: Vec<&str> = u.iter().map(|u| u["sessionUpdate"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["tool_call", "tool_call_update", "agent_message_chunk"]);
    assert_eq!(u[0]["toolCallId"], "t1");
    assert_eq!(u[0]["kind"], "execute");
    assert_eq!(u[0]["status"], "in_progress");
    assert_eq!(u[1]["status"], "completed");
    assert!(u[1]["content"][0]["content"]["text"].as_str().unwrap().contains("from-tool"));
    assert_eq!(u[2]["content"]["text"], "All done.");
    // Every notification names the session.
    assert!(msgs.iter().all(|m| m["params"]["sessionId"] == json!(sid)));
}

#[tokio::test]
async fn an_ask_rule_becomes_a_permission_request() {
    let server = model_server(vec![ok(bash_stream("t1", "echo pushed")), ok(text_stream("ok"))]).await;
    let rule = Rule { tool: "run_bash".into(), pattern: "echo *".into(), action: Action::Ask };
    let mut c = start(&server, vec![rule]);
    let sid = c.new_session().await;
    let pid = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]})).await;
    // A pending tool_call, then the question.
    let mut asked = None;
    while asked.is_none() {
        let m = c.recv().await;
        if m["method"] == "session/request_permission" {
            asked = Some(m);
        } else if m["method"] == "session/update" {
            assert_eq!(m["params"]["update"]["status"], "pending", "{m}");
        }
    }
    let asked = asked.unwrap();
    assert_eq!(asked["params"]["toolCall"]["toolCallId"], "t1");
    let kinds: Vec<&str> = asked["params"]["options"].as_array().unwrap().iter().map(|o| o["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["allow_once", "allow_always", "reject_once"]);
    c.send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}}}))
        .await;
    let (msgs, r) = c.until_response(pid).await;
    assert_eq!(r["result"]["stopReason"], "end_turn");
    let u = updates(&msgs);
    assert_eq!(u[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(u[0]["status"], "in_progress", "the announced call is updated, not created again");
    assert!(u.iter().any(|u| u["status"] == "completed"));
}

#[tokio::test]
async fn cancel_while_asking_ends_the_turn_cancelled() {
    let server = model_server(vec![ok(bash_stream("t1", "echo pushed")), ok(text_stream("never"))]).await;
    let rule = Rule { tool: "run_bash".into(), pattern: "*".into(), action: Action::Ask };
    let mut c = start(&server, vec![rule]);
    let sid = c.new_session().await;
    let pid = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]})).await;
    let asked = loop {
        let m = c.recv().await;
        if m["method"] == "session/request_permission" {
            break m;
        }
    };
    c.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}})).await;
    c.send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": {"outcome": {"outcome": "cancelled"}}})).await;
    let (msgs, r) = c.until_response(pid).await;
    assert_eq!(r["result"]["stopReason"], "cancelled", "{r}");
    let u = updates(&msgs);
    assert!(u.iter().any(|u| u["toolCallId"] == "t1" && u["status"] == "failed"), "{u:?}");
}

#[tokio::test]
async fn load_replays_the_conversation_before_answering() {
    let server = model_server(vec![ok(text_stream("first answer")), ok(text_stream("second"))]).await;
    let tmp = tempfile::tempdir().unwrap();
    let mut c = start_in(&server, vec![], tmp);
    let sid = c.new_session().await;
    c.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "first question"}]})).await;
    let (_, closed) = c.call("session/close", json!({"sessionId": sid})).await;
    assert_eq!(closed["result"], json!({}));
    // No pause needed: close answers once the session's log is released.

    let cwd = c.project();
    let (msgs, r) = c.call("session/load", json!({"sessionId": sid, "cwd": cwd, "mcpServers": []})).await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(r["result"]["modes"]["currentModeId"], "build");
    let u = updates(&msgs);
    assert_eq!(u[0]["sessionUpdate"], "user_message_chunk");
    assert_eq!(u[0]["content"]["text"], "first question");
    assert_eq!(u[1]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(u[1]["content"]["text"], "first answer");
    // And it carries on from there.
    let (_, r) = c.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "and?"}]})).await;
    assert_eq!(r["result"]["stopReason"], "end_turn");

    let (_, missing) = c.call("session/load", json!({"sessionId": "nope", "cwd": cwd, "mcpServers": []})).await;
    assert_eq!(missing["error"]["code"], -32002, "{missing}");
}

#[tokio::test]
async fn set_mode_switches_to_plan_and_says_so() {
    let server = model_server(vec![ok(text_stream("planning"))]).await;
    let mut c = start(&server, vec![]);
    let sid = c.new_session().await;
    let (_, r) = c.call("session/set_mode", json!({"sessionId": sid, "modeId": "plan"})).await;
    assert_eq!(r["result"], json!({}));
    let m = c.recv().await;
    assert_eq!(m["params"]["update"], json!({"sessionUpdate": "current_mode_update", "currentModeId": "plan"}));
    let (_, bad) = c.call("session/set_mode", json!({"sessionId": sid, "modeId": "yolo"})).await;
    assert_eq!(bad["error"]["code"], -32602);
    let (_, missing) = c.call("session/prompt", json!({"sessionId": "nope", "prompt": []})).await;
    assert_eq!(missing["error"]["code"], -32002);
    assert_eq!(missing["error"]["data"]["sessionId"], "nope");
}

#[tokio::test]
async fn images_and_attached_files_reach_the_model() {
    let server = model_server(vec![ok(text_stream("seen"))]).await;
    let mut c = start(&server, vec![]);
    let sid = c.new_session().await;
    let cwd = c.project();
    let prompt = json!([
        {"type": "text", "text": "look at these"},
        {"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"},
        {"type": "resource_link", "uri": format!("file://{cwd}/src/my%20file.rs"), "name": "my file.rs"},
        {"type": "resource", "resource": {"uri": format!("file://{cwd}/notes.md"), "text": "remember this"}},
    ]);
    let (_, r) = c.call("session/prompt", json!({"sessionId": sid, "prompt": prompt})).await;
    assert_eq!(r["result"]["stopReason"], "end_turn");
    let body: Value = serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    let content = &body["messages"][0]["content"];
    assert_eq!(content[0]["type"], "image");
    let text = content[1]["text"].as_str().unwrap();
    assert!(text.contains("look at these"), "{text}");
    assert!(text.contains("src/my file.rs"), "{text}");
    assert!(text.contains("remember this") && text.contains("notes.md"), "{text}");
}

#[tokio::test]
async fn a_failed_turn_is_an_error_response() {
    let server = model_server(vec![ResponseTemplate::new(400).set_body_string("bad request")]).await;
    let mut c = start(&server, vec![]);
    let sid = c.new_session().await;
    let (_, r) = c.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "x"}]})).await;
    assert_eq!(r["error"]["code"], -32603);
    assert!(r["error"]["message"].as_str().unwrap().contains("400"), "{r}");
}

#[tokio::test]
async fn sessions_on_one_project_share_a_container_and_run_side_by_side() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = model_server(vec![ok(text_stream("answer"))]).await;
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let made = Arc::new(AtomicUsize::new(0));
    let counter = made.clone();
    let api_url = format!("{}/v1/messages", server.uri());
    let opts = AcpOptions {
        container: Arc::new(move |dir: &Path, _: &AppConfig| {
            counter.fetch_add(1, Ordering::SeqCst);
            common::fake_container(dir, &bin)
        }),
        config: Arc::new(move |_: &Path| AppConfig { api_url: api_url.clone(), api_key: "t".into(), ..Default::default() }),
        http: reqwest::Client::new(),
        start_containers: false,
    };
    let (client_io, agent_io) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(agent_io);
    tokio::spawn(async move { serve(ar, aw, opts).await.unwrap() });
    let (cr, cw) = tokio::io::split(client_io);
    let mut c = Client { tmp, served: None, write: cw, lines: BufReader::new(cr).lines(), next_id: 0, early: Vec::new() };
    let a = c.new_session().await;
    let cwd = c.project();
    let (_, r) = c.call("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
    let b = r["result"]["sessionId"].as_str().unwrap().to_string();
    assert_ne!(a, b);
    assert_eq!(made.load(Ordering::SeqCst), 1, "one container for the project");
    for sid in [&a, &b] {
        let (_, r) = c.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "hi"}]})).await;
        assert_eq!(r["result"]["stopReason"], "end_turn");
    }
}

// ---- ordering and shutdown (review regressions) ----

fn write_stream(id: &str, path: &str) -> String {
    let input = json!({ "path": path, "content": "x\n" }).to_string();
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":"write_file"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mode_switch_sent_without_waiting_applies_to_the_next_prompt() {
    for _ in 0..5 {
        let server = model_server(vec![ok(write_stream("w", "made.txt")), ok(text_stream("planned"))]).await;
        let mut c = start(&server, vec![]);
        let sid = c.new_session().await;
        c.request("session/set_mode", json!({"sessionId": sid, "modeId": "plan"})).await;
        let pid = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]})).await;
        let (_, r) = c.until_response(pid).await;
        assert_eq!(r["result"]["stopReason"], "end_turn");
        assert!(!c.tmp.path().join("project/made.txt").exists(), "the turn ran in build mode");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pipelined_prompts_run_in_order_and_answer_their_own_requests() {
    for _ in 0..5 {
        let first = ok(text_stream("first")).set_delay(Duration::from_millis(100));
        let server = model_server(vec![first, ok(text_stream("second"))]).await;
        let mut c = start(&server, vec![]);
        let sid = c.new_session().await;
        let a = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "AAAA"}]})).await;
        let b = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "BBBB"}]})).await;
        let texts_before = |msgs: &[Value]| -> String {
            updates(msgs).iter().filter_map(|u| u["content"]["text"].as_str().map(str::to_string)).collect()
        };
        let (msgs_a, ra) = c.until_response(a).await;
        assert_eq!(texts_before(&msgs_a), "first", "A's response came after A's text");
        assert_eq!(ra["result"]["stopReason"], "end_turn");
        let (_, rb) = c.until_response(b).await;
        assert_eq!(rb["result"]["stopReason"], "end_turn");
        let bodies = server.received_requests().await.unwrap();
        let first_prompt: Value = serde_json::from_slice(&bodies[0].body).unwrap();
        assert_eq!(first_prompt["messages"][0]["content"], "AAAA");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_right_after_a_prompt_cancels_it_and_queued_prompts_too() {
    for _ in 0..5 {
        let slow = ok(text_stream("late")).set_delay(Duration::from_secs(5));
        let server = model_server(vec![slow]).await;
        let mut c = start(&server, vec![]);
        let sid = c.new_session().await;
        let a = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "a"}]})).await;
        let b = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "b"}]})).await;
        c.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}})).await;
        let (_, ra) = c.until_response(a).await;
        assert_eq!(ra["result"]["stopReason"], "cancelled", "{ra}");
        let (_, rb) = c.until_response(b).await;
        assert_eq!(rb["result"]["stopReason"], "cancelled", "{rb}");
    }
}

#[tokio::test]
async fn a_bad_line_is_answered_and_the_connection_goes_on() {
    let server = model_server(vec![ok(text_stream("hi"))]).await;
    let mut c = start(&server, vec![]);
    c.write.write_all(b"\xff\xfe\n{not json}\n").await.unwrap();
    assert_eq!(c.recv().await["error"]["code"], -32700);
    assert_eq!(c.recv().await["error"]["code"], -32700);
    let sid = c.new_session().await;
    assert!(!sid.is_empty());
}

#[tokio::test]
async fn closing_stdin_with_a_question_outstanding_still_shuts_down() {
    let server = model_server(vec![ok(bash_stream("t1", "echo x")), ok(text_stream("ok"))]).await;
    let rule = Rule { tool: "*".into(), pattern: "*".into(), action: Action::Ask };
    let mut c = start(&server, vec![rule]);
    let sid = c.new_session().await;
    c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "go"}]})).await;
    loop {
        if c.recv().await["method"] == "session/request_permission" {
            break;
        }
    }
    let served = c.served.take().unwrap();
    drop(c); // the client goes away without answering
    let done = tokio::time::timeout(Duration::from_secs(10), served).await;
    assert!(done.is_ok(), "serve did not return");
}

#[tokio::test]
async fn closing_a_session_cancels_its_queued_prompts() {
    let slow = ok(text_stream("late")).set_delay(Duration::from_secs(5));
    let server = model_server(vec![slow]).await;
    let mut c = start(&server, vec![]);
    let sid = c.new_session().await;
    let a = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "a"}]})).await;
    let b = c.request("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "b"}]})).await;
    c.request("session/close", json!({"sessionId": sid})).await;
    let (_, ra) = c.until_response(a).await;
    assert_eq!(ra["result"]["stopReason"], "cancelled", "{ra}");
    let (_, rb) = c.until_response(b).await;
    assert_eq!(rb["result"]["stopReason"], "cancelled", "{rb}");
}

#[tokio::test]
async fn client_mcp_servers_and_project_commands_reach_the_session() {
    let script = r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("id") is None: continue
    if m["method"] == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "z", "version": "1"}}
    elif m["method"] == "tools/list":
        r = {"tools": [{"name": "now", "description": "The time", "inputSchema": {"type": "object"}}]}
    else:
        r = {"content": [{"type": "text", "text": "noon"}]}
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": r}), flush=True)
"#;
    let call = sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c1","name":"zed__now"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
    ]);
    let server = model_server(vec![ok(call), ok(text_stream("it's noon"))]).await;
    let mut c = start(&server, vec![]);
    let script_path = c.tmp.path().join("zed_mcp.py");
    std::fs::write(&script_path, script).unwrap();
    std::fs::create_dir_all(c.tmp.path().join("project/.syl/commands")).unwrap();
    std::fs::write(c.tmp.path().join("project/.syl/commands/standup.md"), "---\ndescription: Write my standup\n---\nSummarise yesterday.").unwrap();

    c.call("initialize", json!({"protocolVersion": 1})).await;
    let cwd = c.project();
    let mcp = json!([{"name": "zed", "command": "python3", "args": [script_path.to_string_lossy()], "env": []}]);
    let (msgs, r) = c.call("session/new", json!({"cwd": cwd, "mcpServers": mcp})).await;
    let sid = r["result"]["sessionId"].as_str().unwrap().to_string();
    // The commands come after the reply, once the client knows the session.
    assert!(updates(&msgs).iter().all(|u| u["sessionUpdate"] != "available_commands_update"));

    let (msgs, r) = c.call("session/prompt", json!({"sessionId": sid, "prompt": [{"type": "text", "text": "time?"}]})).await;
    let cmds = updates(&msgs).into_iter().find(|u| u["sessionUpdate"] == "available_commands_update").expect("commands offered");
    assert_eq!(cmds["sessionId"].as_str().unwrap_or(&sid), sid);
    assert_eq!(cmds["availableCommands"][0]["name"], "standup");
    assert_eq!(cmds["availableCommands"][0]["description"], "Write my standup");
    assert_eq!(r["result"]["stopReason"], "end_turn");
    let done = updates(&msgs).into_iter().find(|u| u["toolCallId"] == "c1" && u["status"] == "completed").expect("the MCP call finished");
    assert_eq!(done["rawOutput"], "noon");
}
