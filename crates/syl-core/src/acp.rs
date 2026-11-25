//! Syl as an Agent Client Protocol (ACP v1) agent.
//!
//! A client such as Zed or Bondsmith starts `syl acp` and talks to it over
//! stdio: JSON-RPC 2.0, one message per line. Each ACP session is one Syl
//! agent loop with its own session log; sessions on the same project share
//! one container. Agent events map onto `session/update` notifications,
//! and permission questions onto `session/request_permission` requests.
//!
//! Nothing but protocol messages may go to stdout; logs go to the log file.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use syl_container::Container;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::agent::{self, Agent, AgentCommand, AgentEvent, Mode, SessionPaths, SessionStart, TurnEnd};
use crate::api::types::{ContentBlock, ImageSource, MessageContent, StopReason};
use crate::config::AppConfig;
use crate::control::PermissionAnswer;
use crate::model::AnthropicAdapter;
use crate::session::EntryKind;
use crate::shared::SharedState;

pub const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC error codes used here.
mod code {
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const NOT_FOUND: i64 = -32002;
}

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        RpcError { code, message: message.into(), data: None }
    }

    fn no_session(id: &str) -> Self {
        RpcError { code: code::NOT_FOUND, message: format!("no session {id}"), data: Some(json!({ "sessionId": id })) }
    }

    fn to_json(&self) -> Value {
        let mut e = json!({ "code": self.code, "message": self.message });
        if let Some(d) = &self.data {
            e["data"] = d.clone();
        }
        e
    }
}

type RpcResult = Result<Value, RpcError>;

/// Builds the (not yet started) container for a project directory.
pub type ContainerFactory = Arc<dyn Fn(&Path, &AppConfig) -> Container + Send + Sync>;
/// Builds the config for a project directory.
pub type ConfigFactory = Arc<dyn Fn(&Path) -> AppConfig + Send + Sync>;

/// How sessions get their container and config. The binary builds real
/// ones; tests pass a stand-in podman.
pub struct AcpOptions {
    pub container: ContainerFactory,
    pub config: ConfigFactory,
    pub http: reqwest::Client,
    /// Start (or resume) a project's container when its first session opens.
    pub start_containers: bool,
}

/// How a prompt's turn ended, and its last error.
type TurnWaiter = oneshot::Sender<(TurnEnd, Option<String>)>;

/// One open ACP session: a running agent loop.
struct Session {
    commands: mpsc::UnboundedSender<AgentCommand>,
    /// The project directory, for resolving attached file paths.
    cwd: PathBuf,
    /// Waiters for prompts sent, in order: each gets its turn's end and the
    /// last error message of that turn.
    prompts: Mutex<VecDeque<TurnWaiter>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct Server {
    opts: AcpOptions,
    out: mpsc::UnboundedSender<Value>,
    initialized: AtomicBool,
    client_caps: Mutex<Value>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// One container per project, started once; a slow start blocks only
    /// that project's sessions.
    containers: Mutex<HashMap<PathBuf, Arc<tokio::sync::OnceCell<Arc<Container>>>>>,
    next_id: AtomicU64,
    /// Our requests to the client (permission questions), by id.
    outgoing: Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>,
    /// The client has gone: open no more sessions.
    closing: AtomicBool,
}

/// Serve ACP on `reader`/`writer` until the reader closes.
pub async fn serve<R, W>(reader: R, writer: W, opts: AcpOptions) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(msg) = out_rx.recv().await {
            if msg.is_null() {
                break; // shutting down
            }
            // serde_json never emits raw newlines, so one message is one line.
            let mut line = serde_json::to_string(&msg).unwrap_or_default();
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });

    let server = Arc::new(Server {
        opts,
        out,
        initialized: AtomicBool::new(false),
        client_caps: Mutex::new(json!({})),
        sessions: Mutex::new(HashMap::new()),
        containers: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        outgoing: Mutex::new(HashMap::new()),
        closing: AtomicBool::new(false),
    });

    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let read_result: anyhow::Result<()> = loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break Ok(()),
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
        let parsed = std::str::from_utf8(&buf)
            .map_err(|e| e.to_string())
            .and_then(|line| if line.trim().is_empty() { Ok(None) } else { serde_json::from_str::<Value>(line).map(Some).map_err(|e| e.to_string()) });
        match parsed {
            Ok(None) => {}
            Ok(Some(msg)) => server.clone().dispatch(msg),
            // A bad line is answered and skipped; the connection goes on.
            Err(e) => server.send(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})),
        }
    };

    // The client closed stdin (or reading failed): stop every session.
    server.closing.store(true, Ordering::Relaxed);
    // Questions still out get no answer now; dropping them rejects the calls.
    server.outgoing.lock().unwrap().clear();
    let sessions: Vec<Arc<Session>> = server.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
    for s in &sessions {
        let _ = s.commands.send(AgentCommand::Shutdown);
    }
    for s in sessions {
        let task = s.task.lock().unwrap().take();
        if let Some(t) = task {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), t).await;
        }
    }
    // Close the writer once everything queued so far is written, without
    // waiting for every task that might still hold a sender.
    server.send(Value::Null);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), writer_task).await;
    read_result
}

impl Server {
    fn send(&self, msg: Value) {
        let _ = self.out.send(msg);
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn update(&self, session: &str, update: Value) {
        self.notify("session/update", json!({ "sessionId": session, "update": update }));
    }

    /// Send a request to the client and wait for its answer.
    async fn request(&self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.outgoing.lock().unwrap().insert(id, tx);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.await.unwrap_or_else(|_| Err(json!({"message": "connection closed"})))
    }

    fn dispatch(self: Arc<Self>, msg: Value) {
        let id = msg.get("id").cloned().filter(|v| !v.is_null());
        // A response to one of our requests.
        if msg.get("method").is_none() {
            if let Some(n) = id.as_ref().and_then(Value::as_u64) {
                if let Some(tx) = self.outgoing.lock().unwrap().remove(&n) {
                    let _ = tx.send(match msg.get("error") {
                        Some(e) => Err(e.clone()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    });
                }
            }
            return;
        }
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        match id {
            // A notification: no answer.
            None => {
                if method == "session/cancel" {
                    if let Some(s) = params["sessionId"].as_str().and_then(|id| self.session(id)) {
                        let _ = s.commands.send(AgentCommand::Cancel);
                    }
                }
                // Unknown notifications are ignored, as JSON-RPC allows.
            }
            // These are handled here, in the order they arrive, so a client
            // that doesn't wait for replies still gets them applied in its
            // order: initialize before the next request, a mode switch before
            // the prompt after it, prompts in order, cancels after their
            // prompt. Only the waiting for a turn happens elsewhere.
            Some(id) if method == "initialize" => {
                let result = self.initialize(params);
                self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
            }
            Some(id) if matches!(method.as_str(), "session/prompt" | "session/set_mode") && self.initialized.load(Ordering::Relaxed) => {
                let reply = move |r: RpcResult| match r {
                    Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                    Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e.to_json()}),
                };
                if method == "session/set_mode" {
                    let r = self.set_mode(&params);
                    self.send(reply(r));
                    return;
                }
                match self.start_prompt(&params) {
                    Err(e) => self.send(reply(Err(e))),
                    Ok(waiter) => {
                        tokio::spawn(async move {
                            let r = finish_prompt(waiter).await;
                            self.send(reply(r));
                        });
                    }
                }
            }
            Some(id) => {
                tokio::spawn(async move {
                    let result = self.clone().handle(&method, params.clone()).await;
                    // A new session's commands follow its id, once the
                    // client knows that id.
                    let opened = match (&result, method.as_str()) {
                        (Ok(r), "session/new") => r["sessionId"].as_str().map(str::to_string),
                        (Ok(_), "session/load") => params["sessionId"].as_str().map(str::to_string),
                        _ => None,
                    };
                    let reply = match result {
                        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e.to_json()}),
                    };
                    self.send(reply);
                    if let Some(sid) = opened {
                        self.advertise_commands(&sid);
                    }
                });
            }
        }
    }

    /// Slash commands the client can offer: custom commands and skills.
    fn advertise_commands(&self, id: &str) {
        let Some(session) = self.session(id) else { return };
        let cwd = &session.cwd;
        let mut available: Vec<Value> = crate::commands::list_commands(cwd)
            .into_iter()
            .map(|c| json!({ "name": c.name, "description": c.description, "input": { "hint": "what to focus on (optional)" } }))
            .collect();
        available.extend(crate::commands::list_skills(cwd).into_iter().map(|sk| {
            json!({ "name": format!("skill:{}", sk.name), "description": sk.description, "input": { "hint": "the task (optional)" } })
        }));
        if !available.is_empty() {
            self.update(id, json!({ "sessionUpdate": "available_commands_update", "availableCommands": available }));
        }
    }

    fn session(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(id).cloned()
    }

    async fn handle(self: Arc<Self>, method: &str, params: Value) -> RpcResult {
        if method == "initialize" {
            return Ok(self.initialize(params));
        }
        if !self.initialized.load(Ordering::Relaxed) {
            return Err(RpcError::new(code::INVALID_REQUEST, "initialize first"));
        }
        match method {
            "session/new" => self.open(params, None).await,
            "session/load" => {
                let id = str_param(&params, "sessionId")?;
                self.open(params.clone(), Some(id)).await
            }
            "session/prompt" => finish_prompt(self.start_prompt(&params)?).await,
            "session/set_mode" => self.set_mode(&params),
            "session/close" => {
                if let Some(id) = params["sessionId"].as_str() {
                    let removed = self.sessions.lock().unwrap().remove(id);
                    if let Some(s) = removed {
                        let _ = s.commands.send(AgentCommand::Shutdown);
                        // Answer once the loop has stopped and released the
                        // session's log, so loading it again right away works.
                        let task = s.task.lock().unwrap().take();
                        if let Some(t) = task {
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), t).await;
                        }
                    }
                }
                Ok(json!({}))
            }
            other => Err(RpcError::new(code::METHOD_NOT_FOUND, format!("method not found: {other}"))),
        }
    }

    fn require(&self, params: &Value) -> Result<Arc<Session>, RpcError> {
        let id = str_param(params, "sessionId")?;
        self.session(&id).ok_or_else(|| RpcError::no_session(&id))
    }

    fn initialize(&self, params: Value) -> Value {
        *self.client_caps.lock().unwrap() = params.get("clientCapabilities").cloned().unwrap_or(json!({}));
        self.initialized.store(true, Ordering::Relaxed);
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": { "image": true, "audio": false, "embeddedContext": true },
                "mcpCapabilities": { "http": true, "sse": false },
                "sessionCapabilities": { "close": {} },
            },
            "agentInfo": { "name": "syl", "title": "Syl", "version": env!("CARGO_PKG_VERSION") },
            "authMethods": [],
        })
    }

    /// The project's container, created (and started) once per process.
    async fn container_for(&self, dir: &Path, config: &AppConfig) -> Result<Arc<Container>, RpcError> {
        let cell = self.containers.lock().unwrap().entry(dir.to_path_buf()).or_default().clone();
        cell.get_or_try_init(|| async {
            let container = Arc::new((self.opts.container)(dir, config));
            if self.opts.start_containers {
                let c = container.clone();
                let started = tokio::task::spawn_blocking(move || {
                    c.start_or_resume(false, &|msg| tracing::info!("container: {msg}"))
                })
                .await;
                match started {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(RpcError::new(code::INTERNAL, format!("could not start the container: {e}"))),
                    Err(e) => return Err(RpcError::new(code::INTERNAL, format!("could not start the container: {e}"))),
                }
            }
            Ok(container)
        })
        .await
        .cloned()
    }

    /// `session/new` (resume = None) and `session/load`.
    async fn open(self: Arc<Self>, params: Value, resume: Option<String>) -> RpcResult {
        let cwd = str_param(&params, "cwd")?;
        let cwd = PathBuf::from(&cwd);
        if !cwd.is_absolute() {
            return Err(RpcError::new(code::INVALID_PARAMS, "cwd must be an absolute path"));
        }
        let cwd = cwd.canonicalize().map_err(|e| RpcError::new(code::INVALID_PARAMS, format!("cwd: {e}")))?;
        let client_mcp = client_mcp_servers(&params["mcpServers"])?;
        if let Some(id) = &resume {
            if self.session(id).is_some() {
                return Err(RpcError::new(code::INVALID_PARAMS, format!("session {id} is already open")));
            }
        }
        let mut config = (self.opts.config)(&cwd);
        // The client's own servers run where the client says (on the host),
        // and replace a configured server of the same name.
        config.mcp.retain(|s| !client_mcp.iter().any(|c| c.name == s.name));
        config.mcp.extend(client_mcp);
        let container = self.container_for(&cwd, &config).await?;

        let state = SharedState::new();
        *state.current_model.lock().unwrap() = config.model.clone();
        let model = Arc::new(AnthropicAdapter::new(self.opts.http.clone(), state.clone()));
        let paths = SessionPaths::for_project(&cwd);
        let sessions_dir = paths.sessions_dir.clone();
        let mut handle = agent::spawn(Agent {
            config,
            state,
            model,
            container,
            paths,
            start: match &resume {
                Some(id) => SessionStart::Resume(id.clone()),
                None => SessionStart::New,
            },
        });

        // The loop starts by reporting its session (after an Error if a
        // resume failed and it started a new one instead).
        let mut resume_error = None;
        let info = loop {
            match handle.events.recv().await {
                Some(AgentEvent::SessionChanged { info }) => break info,
                Some(AgentEvent::Error { message }) => resume_error = Some(message),
                Some(_) => {}
                None => return Err(RpcError::new(code::INTERNAL, "the agent stopped")),
            }
        };
        if let (Some(id), Some(err)) = (&resume, resume_error) {
            let _ = handle.commands.send(AgentCommand::Shutdown);
            let mut e = RpcError::no_session(id);
            e.message = err;
            return Err(e);
        }
        let id = info.id.clone();
        if self.closing.load(Ordering::Relaxed) {
            let _ = handle.commands.send(AgentCommand::Shutdown);
            return Err(RpcError::new(code::INTERNAL, "shutting down"));
        }

        // session/load: replay the conversation before answering.
        if resume.is_some() {
            if let Some(dir) = &sessions_dir {
                if let Ok(entries) = crate::session::load_entries(dir, &id) {
                    replay(&self, &id, &cwd, &entries);
                }
            }
        }

        let session = Arc::new(Session {
            commands: handle.commands.clone(),
            cwd: cwd.clone(),
            prompts: Mutex::new(VecDeque::new()),
            task: Mutex::new(None),
        });
        self.sessions.lock().unwrap().insert(id.clone(), session.clone());
        let pump = tokio::spawn(pump(self.clone(), session.clone(), id.clone(), cwd.clone(), handle.events));
        // Keep the agent task too: it finishes when the loop stops.
        let agent_task = handle.task;
        *session.task.lock().unwrap() = Some(tokio::spawn(async move {
            let _ = agent_task.await;
            let _ = pump.await;
        }));

        let mut result = json!({ "modes": modes(Mode::Build) });
        if resume.is_none() {
            result["sessionId"] = json!(id);
        }
        Ok(result)
    }

    fn set_mode(&self, params: &Value) -> RpcResult {
        let session = self.require(params)?;
        let mode = match params["modeId"].as_str() {
            Some("plan") => Mode::Plan,
            Some("build") => Mode::Build,
            other => return Err(RpcError::new(code::INVALID_PARAMS, format!("unknown mode {other:?}"))),
        };
        let _ = session.commands.send(AgentCommand::SetMode(mode));
        Ok(json!({}))
    }

    /// Queue a prompt: its waiter and its command go in together, under the
    /// session's prompt lock, so waiters and turns pair up in order.
    fn start_prompt(&self, params: &Value) -> Result<oneshot::Receiver<(TurnEnd, Option<String>)>, RpcError> {
        let session = self.require(params)?;
        let blocks = params["prompt"]
            .as_array()
            .ok_or_else(|| RpcError::new(code::INVALID_PARAMS, "prompt must be an array of content blocks"))?;
        let (text, images) = prompt_to_syl(blocks, &session.cwd);
        if text.trim().is_empty() && images.is_empty() {
            return Err(RpcError::new(code::INVALID_PARAMS, "the prompt is empty"));
        }
        let cmd = if images.is_empty() {
            AgentCommand::Prompt(text)
        } else {
            AgentCommand::PromptWithImages { text, images }
        };
        let (tx, rx) = oneshot::channel();
        let mut prompts = session.prompts.lock().unwrap();
        if session.commands.send(cmd).is_err() {
            return Err(RpcError::new(code::INTERNAL, "the session has stopped"));
        }
        prompts.push_back(tx);
        Ok(rx)
    }
}

/// Wait for a queued prompt's turn and turn it into the response.
async fn finish_prompt(rx: oneshot::Receiver<(TurnEnd, Option<String>)>) -> RpcResult {
    {
        let (end, error) = rx.await.map_err(|_| RpcError::new(code::INTERNAL, "the session stopped mid-turn"))?;
        let stop = match end {
            TurnEnd::Completed(StopReason::MaxTokens | StopReason::ContextWindowExceeded) => "max_tokens",
            TurnEnd::Completed(StopReason::Other(r)) if r == "refusal" => "refusal",
            TurnEnd::Completed(_) => "end_turn",
            TurnEnd::Cancelled => "cancelled",
            TurnEnd::Failed => {
                return Err(RpcError::new(code::INTERNAL, error.unwrap_or_else(|| "the turn failed".to_string())));
            }
        };
        Ok(json!({ "stopReason": stop }))
    }
}

/// ACP `mcpServers` as Syl's server configs. They come from the client,
/// which runs Syl, so stdio servers run on the host as it asks.
fn client_mcp_servers(list: &Value) -> Result<Vec<crate::mcp::McpServerConfig>, RpcError> {
    let pairs = |v: &Value| -> std::collections::BTreeMap<String, String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| Some((p["name"].as_str()?.to_string(), p["value"].as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let Some(items) = list.as_array() else { return Ok(Vec::new()) };
    items
        .iter()
        .map(|m| {
            let name = m["name"].as_str().ok_or_else(|| RpcError::new(code::INVALID_PARAMS, "an MCP server has no name"))?;
            let mut cfg = crate::mcp::McpServerConfig {
                name: name.to_string(),
                command: None,
                args: Vec::new(),
                env: Default::default(),
                url: None,
                headers: Default::default(),
                host: true,
                timeout_secs: None,
            };
            match m["type"].as_str() {
                Some("http") => {
                    cfg.url = m["url"].as_str().map(str::to_string);
                    cfg.headers = pairs(&m["headers"]);
                }
                Some("sse") => {
                    return Err(RpcError::new(code::INVALID_PARAMS, format!("MCP server {name}: SSE transport isn't supported")))
                }
                _ => {
                    cfg.command = m["command"].as_str().map(str::to_string);
                    cfg.args = m["args"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    cfg.env = pairs(&m["env"]);
                }
            }
            Ok(cfg)
        })
        .collect()
}

fn str_param(params: &Value, key: &str) -> Result<String, RpcError> {
    params[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| RpcError::new(code::INVALID_PARAMS, format!("missing {key}")))
}

fn modes(current: Mode) -> Value {
    json!({
        "currentModeId": current.name(),
        "availableModes": [
            { "id": "build", "name": "Build", "description": "Syl reads, runs commands and edits files." },
            { "id": "plan", "name": "Plan", "description": "Syl reads and runs commands but can't change files; anything a command changes is put back." },
        ],
    })
}

/// ACP prompt content to Syl's: text (with attachments described) and
/// images.
pub fn prompt_to_syl(blocks: &[Value], cwd: &Path) -> (String, Vec<ImageSource>) {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("text") => text.push(b["text"].as_str().unwrap_or("").to_string()),
            Some("image") => {
                if let (Some(data), Some(mime)) = (b["data"].as_str(), b["mimeType"].as_str()) {
                    images.push(ImageSource::base64(mime, data));
                }
            }
            Some("resource_link") => {
                let uri = b["uri"].as_str().unwrap_or("");
                text.push(match workspace_path(uri, cwd) {
                    Some(rel) => format!("[Attached file: {rel} — read it with read_file]"),
                    None => format!("[Linked: {uri}]"),
                });
            }
            Some("resource") => {
                let r = &b["resource"];
                let uri = r["uri"].as_str().unwrap_or("");
                let name = workspace_path(uri, cwd).unwrap_or_else(|| uri.to_string());
                match r["text"].as_str() {
                    Some(t) => text.push(format!("<attached-file path=\"{name}\">\n{t}\n</attached-file>")),
                    None => text.push(format!("[Attached binary file: {name} (not shown)]")),
                }
            }
            _ => {}
        }
    }
    (text.join("\n\n"), images)
}

/// A `file://` URI (or absolute path) under the project, relative to it.
fn workspace_path(uri: &str, cwd: &Path) -> Option<String> {
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    let path = percent_decode(path);
    let rel = Path::new(&path).strip_prefix(cwd).ok()?;
    Some(rel.to_string_lossy().into_owned())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// ACP's tool kind for a Syl tool.
fn tool_kind(name: &str) -> &'static str {
    match name {
        "read_file" | "show_image" | "memory_read" => "read",
        "edit_file" | "write_file" | "memory_write" | "workflow_write" => "edit",
        "workflow_delete" => "delete",
        "grep" | "glob" | "ls" => "search",
        "run_bash" => "execute",
        "dispatch_agent" => "fetch",
        _ => "other",
    }
}

/// A one-line title for a tool call.
fn tool_title(name: &str, input: &Value) -> String {
    let arg = |k: &str| input[k].as_str().unwrap_or("").to_string();
    let detail = match name {
        "run_bash" => arg("command"),
        "read_file" | "edit_file" | "write_file" | "show_image" | "ls" => arg("path"),
        "grep" | "glob" => arg("pattern"),
        "dispatch_agent" => arg("source"),
        "memory_read" | "memory_write" => arg("key"),
        _ => String::new(),
    };
    let detail = crate::session::one_line(&detail, 120);
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {detail}")
    }
}

/// Where on the host a tool call looks, for the client to show.
fn tool_locations(name: &str, input: &Value, cwd: &Path) -> Value {
    let Some(path) = input["path"].as_str() else { return json!([]) };
    if !matches!(name, "read_file" | "edit_file" | "write_file" | "ls" | "show_image" | "grep" | "glob") {
        return json!([]);
    }
    let rel = crate::permissions::normalize_path(path);
    if rel.starts_with('/') || rel == ".." || rel.starts_with("../") {
        return json!([]); // outside the workspace: no host path
    }
    let line = input["start_line"].as_u64();
    let host = if rel == "." { cwd.to_path_buf() } else { cwd.join(rel) };
    json!([{ "path": host.to_string_lossy(), "line": line }])
}

fn tool_call(id: &str, name: &str, input: &Value, status: &str, cwd: &Path) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id,
        "title": tool_title(name, input),
        "name": name,
        "kind": tool_kind(name),
        "status": status,
        "rawInput": input,
        "locations": tool_locations(name, input, cwd),
    })
}

fn tool_result(id: &str, output: &str, is_error: bool) -> Value {
    json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": if is_error { "failed" } else { "completed" },
        "content": [{ "type": "content", "content": { "type": "text", "text": output } }],
        "rawOutput": output,
    })
}

/// `session/load`: the conversation as `session/update`s, from the log.
fn replay(server: &Server, session: &str, cwd: &Path, entries: &[crate::session::Entry]) {
    let text = |t: &str| json!({ "type": "text", "text": t });
    for e in entries {
        match &e.kind {
            EntryKind::Message { message } => {
                let role_update = if message.role == "user" { "user_message_chunk" } else { "agent_message_chunk" };
                match &message.content {
                    MessageContent::Text(t) => {
                        server.update(session, json!({ "sessionUpdate": role_update, "content": text(t), "messageId": e.id }))
                    }
                    MessageContent::Blocks(blocks) => {
                        for b in blocks {
                            match b {
                                ContentBlock::Text { text: t, .. } => server.update(
                                    session,
                                    json!({ "sessionUpdate": role_update, "content": text(t), "messageId": e.id }),
                                ),
                                ContentBlock::ToolUse { id, name, input } => {
                                    server.update(session, tool_call(id, name, input, "in_progress", cwd))
                                }
                                ContentBlock::Image { source } => server.update(
                                    session,
                                    json!({ "sessionUpdate": role_update, "messageId": e.id,
                                            "content": { "type": "image", "data": source.data, "mimeType": source.media_type } }),
                                ),
                                _ => {}
                            }
                        }
                    }
                }
            }
            EntryKind::ToolResult { tool_use_id, content, is_error } => {
                server.update(session, tool_result(tool_use_id, content, *is_error))
            }
            EntryKind::Steer { text: t } => {
                server.update(session, json!({ "sessionUpdate": "user_message_chunk", "content": text(t), "messageId": e.id }))
            }
            _ => {}
        }
    }
}

/// Forward one session's agent events to the client.
async fn pump(
    server: Arc<Server>,
    session: Arc<Session>,
    id: String,
    cwd: PathBuf,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
) {
    // Tool calls the client already knows about (announced while asking).
    let mut announced: HashSet<String> = HashSet::new();
    let mut last_error: Option<String> = None;
    let notices = server.client_caps.lock().unwrap()["session"]["notices"].is_object();
    while let Some(ev) = events.recv().await {
        match ev {
            AgentEvent::TextDelta { text } => server.update(
                &id,
                json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } }),
            ),
            AgentEvent::ThinkingDelta { text } => server.update(
                &id,
                json!({ "sessionUpdate": "agent_thought_chunk", "content": { "type": "text", "text": text } }),
            ),
            AgentEvent::ToolCallStarted { id: call, name, input } => {
                if announced.remove(&call) {
                    server.update(&id, json!({ "sessionUpdate": "tool_call_update", "toolCallId": call, "status": "in_progress" }));
                } else {
                    server.update(&id, tool_call(&call, &name, &input, "in_progress", &cwd));
                }
            }
            AgentEvent::ToolCallFinished { id: call, output, is_error, .. } => {
                announced.remove(&call);
                server.update(&id, tool_result(&call, &output, is_error));
            }
            AgentEvent::PermissionRequested { id: call, name, input, subject, note } => {
                server.update(&id, tool_call(&call, &name, &input, "pending", &cwd));
                announced.insert(call.clone());
                let server2 = server.clone();
                let commands = session.commands.clone();
                let sid = id.clone();
                let title = match &note {
                    Some(n) => format!("{} — asks because of: {subject}. {n}", tool_title(&name, &input)),
                    None => format!("{} — asks because of: {subject}", tool_title(&name, &input)),
                };
                tokio::spawn(async move {
                    let params = json!({
                        "sessionId": sid,
                        "toolCall": { "toolCallId": call, "title": title, "kind": tool_kind(&name), "status": "pending", "rawInput": input },
                        "options": [
                            { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                            { "optionId": "allow_always", "name": "Always allow this (in this project)", "kind": "allow_always" },
                            { "optionId": "reject_once", "name": "Reject", "kind": "reject_once" },
                        ],
                    });
                    let answer = match server2.request("session/request_permission", params).await {
                        Ok(r) if r["outcome"]["outcome"] == "selected" => match r["outcome"]["optionId"].as_str() {
                            Some("allow_once") => PermissionAnswer::Once,
                            Some("allow_always") => PermissionAnswer::Always,
                            // Anything else, unknown ids included, is a no.
                            _ => PermissionAnswer::Reject,
                        },
                        _ => PermissionAnswer::Reject,
                    };
                    let _ = commands.send(AgentCommand::ApprovePermission { id: call, answer });
                });
            }
            AgentEvent::ModeChanged { mode } => {
                server.update(&id, json!({ "sessionUpdate": "current_mode_update", "currentModeId": mode.name() }))
            }
            AgentEvent::Error { message } => {
                tracing::warn!(session = %id, "agent error: {message}");
                last_error = Some(message);
            }
            AgentEvent::Notice { message } => {
                tracing::info!(session = %id, "notice: {message}");
                if notices {
                    server.update(
                        &id,
                        json!({ "sessionUpdate": "notice", "severity": "info", "title": crate::session::one_line(&message, 200) }),
                    );
                }
            }
            AgentEvent::TurnEnded { reason } => {
                let waiter = session.prompts.lock().unwrap().pop_front();
                if let Some(tx) = waiter {
                    let _ = tx.send((reason, last_error.take()));
                }
                last_error = None;
            }
            _ => {}
        }
    }
    // The loop stopped (session/close, or the client went away): prompts
    // still waiting were cancelled, not failed.
    for tx in session.prompts.lock().unwrap().drain(..) {
        let _ = tx.send((TurnEnd::Cancelled, None));
    }
}
