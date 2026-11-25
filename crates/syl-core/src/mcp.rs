//! A Model Context Protocol client: Syl can use the tools MCP servers offer.
//!
//! ```toml
//! [[mcp]]
//! name = "github"                       # tools appear as github__<tool>
//! command = "npx"                       # a stdio server...
//! args = ["-y", "@modelcontextprotocol/server-github"]
//! env = { GITHUB_TOKEN = "..." }
//!
//! [[mcp]]
//! name = "docs"
//! url = "https://example.com/mcp"       # ...or a Streamable HTTP server
//! headers = { Authorization = "Bearer ..." }
//! ```
//!
//! Stdio servers run **in the container**, like the agent's commands. A
//! server on the host (a browser, say) is possible with `host = true`, but
//! only in `~/.syl/config.toml`: a project's `.syl/config.toml` comes with
//! the project, and a cloned repository must not be able to run programs on
//! your machine. ACP clients (Zed, Bondsmith) may pass their own servers,
//! which run on the host as the client asked.
//!
//! Servers start with the first turn. Their tools are fixed for the
//! conversation, with the system prompt, so the prompt cache holds.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use syl_container::Container;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// The MCP protocol version Syl speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    /// A stdio server: the program to run.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// A Streamable HTTP server.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Run a stdio server on the host instead of in the container. Only
    /// honoured from `~/.syl/config.toml` or an ACP client.
    #[serde(default)]
    pub host: bool,
    /// Seconds a tool call may take. Default 120.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// A tool a server offers.
#[derive(Debug, Clone)]
struct RemoteTool {
    server: usize,
    /// The server's name for it.
    name: String,
    read_only: bool,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// How to reach one server.
enum Transport {
    Stdio {
        out: mpsc::UnboundedSender<String>,
        pending: Pending,
        /// False once the reader stopped (the server exited or wrote
        /// something unreadable): requests then fail at once.
        alive: Arc<std::sync::atomic::AtomicBool>,
        _child: Arc<tokio::sync::Mutex<tokio::process::Child>>,
    },
    Http {
        http: reqwest::Client,
        url: String,
        headers: BTreeMap<String, String>,
        session: Mutex<Option<String>>,
    },
}

struct Server {
    name: String,
    transport: Transport,
    next_id: AtomicU64,
    timeout: Duration,
}

impl Server {
    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        match &self.transport {
            Transport::Stdio { out, pending, alive, .. } => {
                if !alive.load(Ordering::Relaxed) {
                    return Err("the server has stopped".to_string());
                }
                let (tx, rx) = oneshot::channel();
                pending.lock().unwrap().insert(id, tx);
                if out.send(msg.to_string()).is_err() {
                    pending.lock().unwrap().remove(&id);
                    return Err("the server has stopped".to_string());
                }
                let answer = tokio::time::timeout(timeout, rx).await;
                pending.lock().unwrap().remove(&id);
                match answer {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) => Err("the server has stopped".to_string()),
                    Err(_) => Err(format!("no answer within {}s", timeout.as_secs())),
                }
            }
            Transport::Http { .. } => tokio::time::timeout(timeout, self.http_post(&msg, Some(id)))
                .await
                .unwrap_or_else(|_| Err(format!("no answer within {}s", timeout.as_secs()))),
        }
    }

    async fn notify(&self, method: &str) {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match &self.transport {
            Transport::Stdio { out, .. } => {
                let _ = out.send(msg.to_string());
            }
            Transport::Http { .. } => {
                let _ = self.http_post(&msg, None).await;
            }
        }
    }

    /// POST one message; for a request, read the response: JSON, or an SSE
    /// stream read as it arrives (it may carry the server's own requests
    /// before the response, which are answered).
    async fn http_post(&self, msg: &Value, id: Option<u64>) -> Result<Value, String> {
        let Transport::Http { http, url, headers, session } = &self.transport else { unreachable!() };
        let resp = self.http_send(http, url, headers, session, msg).await?;
        let status = resp.status();
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *session.lock().unwrap() = Some(s.to_string());
        }
        let is_sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.starts_with("text/event-stream"));
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("HTTP {status}: {}", crate::session::one_line(&body, 200)));
        }
        let Some(id) = id else { return Ok(Value::Null) };
        if !is_sse {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            let v: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let found = match v {
                Value::Array(items) => items.into_iter().find(|m| m["id"] == json!(id)),
                m => Some(m),
            };
            return found.map(response_result).unwrap_or_else(|| Err("the server sent no response".to_string()));
        }
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut events = SseEvents::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| e.to_string())?;
            for data in events.feed(&chunk) {
                let Ok(m) = serde_json::from_str::<Value>(&data) else { continue };
                if m.get("method").is_some() {
                    // A request from the server: answer it (only ping is supported).
                    if let Some(rid) = m.get("id").filter(|i| !i.is_null()) {
                        let reply = if m["method"] == "ping" {
                            json!({"jsonrpc": "2.0", "id": rid, "result": {}})
                        } else {
                            json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "not supported"}})
                        };
                        let _ = self.http_send(http, url, headers, session, &reply).await;
                    }
                    continue;
                }
                if m["id"] == json!(id) {
                    return response_result(m);
                }
            }
        }
        Err("the server sent no response".to_string())
    }

    async fn http_send(
        &self,
        http: &reqwest::Client,
        url: &str,
        headers: &BTreeMap<String, String>,
        session: &Mutex<Option<String>>,
        msg: &Value,
    ) -> Result<reqwest::Response, String> {
        let mut req = http
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .json(msg);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        if let Some(s) = session.lock().unwrap().clone() {
            req = req.header("mcp-session-id", s);
        }
        req.send().await.map_err(|e| e.to_string())
    }
}

/// Server-sent events: each event's `data:` lines joined with newlines.
#[derive(Default)]
struct SseEvents {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseEvents {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    out.push(std::mem::take(&mut self.data).join("\n"));
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
        }
        out
    }
}

fn response_result(m: Value) -> Result<Value, String> {
    match m.get("error") {
        Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
        None => Ok(m.get("result").cloned().unwrap_or(Value::Null)),
    }
}

/// Start a stdio server process and the tasks that read and write it.
fn spawn_stdio(mut cmd: tokio::process::Command) -> anyhow::Result<Transport> {
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
    let pending: Pending = Default::default();
    tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });
    let reader_pending = pending.clone();
    let reply_out = out.clone();
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let reader_alive = alive.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // A line that isn't JSON (a stray log line, bad bytes) is skipped.
            let Ok(msg) = serde_json::from_slice::<Value>(&line) else { continue };
            match (msg.get("id").filter(|i| !i.is_null()), msg.get("method")) {
                // A request from the server: Syl offers no client features
                // (sampling, roots, elicitation) beyond ping.
                (Some(id), Some(method)) => {
                    let reply = if method == "ping" {
                        json!({"jsonrpc": "2.0", "id": id, "result": {}})
                    } else {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "not supported"}})
                    };
                    let _ = reply_out.send(reply.to_string());
                }
                (Some(id), None) => {
                    if let Some(tx) = id.as_u64().and_then(|n| reader_pending.lock().unwrap().remove(&n)) {
                        let _ = tx.send(response_result(msg));
                    }
                }
                _ => {} // notifications: the tool list is fixed per conversation
            }
        }
        // The server went away: fail whatever was waiting, and what comes next.
        reader_alive.store(false, Ordering::Relaxed);
        reader_pending.lock().unwrap().clear();
    });
    Ok(Transport::Stdio { out, pending, alive, _child: Arc::new(tokio::sync::Mutex::new(child)) })
}

/// The tools of every configured server, ready for the model.
#[derive(Default)]
pub struct McpHub {
    servers: Vec<Server>,
    tools: HashMap<String, RemoteTool>,
    /// Tool definitions in the Messages API's shape.
    pub definitions: Vec<Value>,
    /// Server instructions, for the system prompt.
    pub instructions: Vec<(String, String)>,
}

/// `server__tool`, in the characters and length the API allows.
fn exposed_name(server: &str, tool: &str, taken: &HashSet<String>) -> String {
    let clean = |s: &str| -> String {
        s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
    };
    let mut name: String = format!("{}__{}", clean(server), clean(tool)).chars().take(64).collect();
    let mut n = 2;
    while taken.contains(&name) {
        let suffix = format!("_{n}");
        name = format!("{}{suffix}", name.chars().take(64 - suffix.len()).collect::<String>());
        n += 1;
    }
    name
}

impl McpHub {
    /// Connect to every server, at most `wait` each. Servers that fail are
    /// left out and reported.
    pub async fn start(
        configs: &[McpServerConfig],
        container: &Container,
        project: &Path,
        http: &reqwest::Client,
        wait: Duration,
    ) -> (McpHub, Vec<String>) {
        let mut hub = McpHub::default();
        let mut problems = Vec::new();
        // All at once, each within `wait` (tools/list pages included).
        let connected = futures_util::future::join_all(configs.iter().map(|cfg| async move {
            tokio::time::timeout(wait, connect(cfg, container, project, http, wait))
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("no answer within {}s", wait.as_secs())))
        }))
        .await;
        for (cfg, result) in configs.iter().zip(connected) {
            match result {
                Ok((server, tools, instructions)) => {
                    let idx = hub.servers.len();
                    if let Some(text) = instructions.filter(|t| !t.trim().is_empty()) {
                        hub.instructions.push((cfg.name.clone(), text));
                    }
                    let mut taken: HashSet<String> = hub.tools.keys().cloned().collect();
                    for t in tools {
                        let Some(tool_name) = t["name"].as_str() else { continue };
                        let name = exposed_name(&cfg.name, tool_name, &taken);
                        taken.insert(name.clone());
                        let read_only = t["annotations"]["readOnlyHint"].as_bool() == Some(true);
                        let schema = api_schema(&t["inputSchema"]);
                        let description = format!(
                            "[MCP server {}] {}",
                            cfg.name,
                            t["description"].as_str().unwrap_or(t["title"].as_str().unwrap_or(tool_name))
                        );
                        hub.definitions.push(json!({"name": name, "description": description, "input_schema": schema}));
                        hub.tools.insert(name, RemoteTool { server: idx, name: tool_name.to_string(), read_only });
                    }
                    hub.servers.push(server);
                }
                Err(e) => problems.push(format!("MCP server {} didn't start: {e}", cfg.name)),
            }
        }
        (hub, problems)
    }

    pub fn has(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Tools that don't say they only read. Plan mode refuses them.
    pub fn mutating(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tools.iter().filter(|(_, t)| !t.read_only).map(|(n, _)| n.clone()).collect();
        v.sort();
        v
    }

    pub fn is_read_only(&self, name: &str) -> bool {
        self.tools.get(name).is_some_and(|t| t.read_only)
    }

    /// Call a tool. Returns its result as text, and whether it's an error.
    pub async fn call(&self, name: &str, input: &Value, cancel: &CancellationToken) -> (String, bool) {
        let Some(tool) = self.tools.get(name) else {
            return (format!("[Tool error: unknown tool `{name}`]"), true);
        };
        let server = &self.servers[tool.server];
        let call = server.request("tools/call", json!({"name": tool.name, "arguments": input}), server.timeout);
        let result = tokio::select! {
            r = call => r,
            _ = cancel.cancelled() => return ("[Tool error: cancelled by user]".to_string(), true),
        };
        match result {
            Err(e) => (format!("[Tool error: MCP server {}: {e}]", server.name), true),
            Ok(r) => (result_text(&r), r["isError"].as_bool().unwrap_or(false)),
        }
    }
}

/// A tool's input schema in a form the Messages API accepts: an object
/// schema without top-level combinators or references (it rejects those,
/// and one bad tool would fail every request of the conversation). What
/// can't be kept is loosened, not dropped: the tool still works.
pub fn api_schema(schema: &Value) -> Value {
    let mut s = match schema {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    s.insert("type".into(), json!("object"));
    let loosened = ["oneOf", "anyOf", "allOf", "not", "$ref", "if", "then", "else"]
        .iter()
        .filter(|k| s.remove(**k).is_some())
        .count()
        > 0;
    if !s.get("properties").is_some_and(Value::is_object) {
        s.insert("properties".into(), json!({}));
    }
    if loosened {
        s.remove("required");
        s.insert("additionalProperties".into(), json!(true));
    }
    Value::Object(s)
}

/// An MCP tool result as text for the model.
pub fn result_text(r: &Value) -> String {
    let parts: Vec<String> = r["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|c| match c["type"].as_str() {
                    Some("text") => c["text"].as_str().unwrap_or("").to_string(),
                    Some("image") => format!("[image: {}]", c["mimeType"].as_str().unwrap_or("?")),
                    Some("audio") => format!("[audio: {}]", c["mimeType"].as_str().unwrap_or("?")),
                    Some("resource") => match c["resource"]["text"].as_str() {
                        Some(t) => t.to_string(),
                        None => format!("[resource: {}]", c["resource"]["uri"].as_str().unwrap_or("?")),
                    },
                    Some("resource_link") => format!("[link: {}]", c["uri"].as_str().unwrap_or("?")),
                    _ => c.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    if parts.is_empty() {
        r.get("structuredContent").map(|s| s.to_string()).unwrap_or_default()
    } else {
        parts.join("\n")
    }
}

async fn connect(
    cfg: &McpServerConfig,
    container: &Container,
    project: &Path,
    http: &reqwest::Client,
    wait: Duration,
) -> anyhow::Result<(Server, Vec<Value>, Option<String>)> {
    let transport = match (&cfg.command, &cfg.url) {
        (Some(command), None) => {
            let env: Vec<(String, String)> = cfg.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let cmd = if cfg.host {
                let mut c = tokio::process::Command::new(command);
                c.args(&cfg.args).envs(env).current_dir(project);
                c
            } else {
                let mut argv: Vec<&str> = vec![command.as_str()];
                argv.extend(cfg.args.iter().map(String::as_str));
                container.interactive(&argv, &env)
            };
            spawn_stdio(cmd)?
        }
        (None, Some(url)) => Transport::Http {
            http: http.clone(),
            url: url.clone(),
            headers: cfg.headers.clone(),
            session: Mutex::new(None),
        },
        _ => anyhow::bail!("give either `command` (stdio) or `url` (HTTP), not both or neither"),
    };
    let server = Server {
        name: cfg.name.clone(),
        transport,
        next_id: AtomicU64::new(1),
        timeout: Duration::from_secs(cfg.timeout_secs.unwrap_or(120)),
    };
    let init = server
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "syl", "version": env!("CARGO_PKG_VERSION")},
            }),
            wait,
        )
        .await
        .map_err(|e| anyhow::anyhow!("initialize: {e}"))?;
    server.notify("notifications/initialized").await;
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let page = server.request("tools/list", params, wait).await.map_err(|e| anyhow::anyhow!("tools/list: {e}"))?;
        tools.extend(page["tools"].as_array().cloned().unwrap_or_default());
        cursor = page["nextCursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    if let Some(v) = init["protocolVersion"].as_str().filter(|v| *v != PROTOCOL_VERSION) {
        tracing::info!(server = %cfg.name, "MCP server speaks protocol {v}; Syl speaks {PROTOCOL_VERSION}");
    }
    let instructions = init["instructions"].as_str().map(str::to_string);
    Ok((server, tools, instructions))
}

/// The system prompt section for server instructions.
pub fn instructions_prompt(hub: &McpHub) -> Option<String> {
    if hub.instructions.is_empty() {
        return None;
    }
    let mut s = String::from("\n### MCP SERVERS\nInstructions from the MCP servers whose tools you have (named <server>__<tool>):\n");
    for (name, text) in &hub.instructions {
        s.push_str(&format!("\n#### {name}\n{}\n", crate::util::truncate_output(text.trim())));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fit_the_api() {
        let taken = HashSet::new();
        assert_eq!(exposed_name("git hub", "list.issues", &taken), "git_hub__list_issues");
        let long = exposed_name(&"s".repeat(40), &"t".repeat(40), &taken);
        assert_eq!(long.len(), 64);
        let taken: HashSet<String> = [long.clone()].into();
        let again = exposed_name(&"s".repeat(40), &"t".repeat(40), &taken);
        assert!(again.ends_with("_2") && again.len() <= 64);
    }

    #[test]
    fn schemas_are_made_acceptable() {
        assert_eq!(api_schema(&json!({"properties": {"a": {"type": "string"}}})),
                   json!({"type": "object", "properties": {"a": {"type": "string"}}}));
        let loose = api_schema(&json!({"type": "object", "anyOf": [{"required": ["a"]}], "required": ["a"]}));
        assert_eq!(loose, json!({"type": "object", "properties": {}, "additionalProperties": true}));
        assert_eq!(api_schema(&json!(null)), json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn sse_events_join_their_data_lines() {
        let mut e = SseEvents::default();
        assert!(e.feed(b"event: message\ndata: {\"a\":\ndata: 1}\n").is_empty());
        assert_eq!(e.feed(b"\n"), ["{\"a\":\n1}"]);
    }

    #[test]
    fn results_become_text() {
        let r = json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "data": "x", "mimeType": "image/png"},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}}
        ]});
        assert_eq!(result_text(&r), "hello\n[image: image/png]\nbody");
        assert_eq!(result_text(&json!({"content": [], "structuredContent": {"n": 1}})), "{\"n\":1}");
    }
}
