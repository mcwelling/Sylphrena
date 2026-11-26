//! The agent loop, driven by commands and observed through events.
//!
//! `spawn` starts the loop on a task. A client sends `AgentCommand`s and
//! reads `AgentEvent`s; it never needs a terminal. Every `Prompt` produces
//! `TurnStarted`, then any number of other events, then exactly one
//! `TurnEnded`. Session commands answer with `SessionChanged`, or `Error`.
//!
//! The loop owns the session log (see `session`): every prompt, assistant
//! turn and tool result is appended and fsynced as it happens, and the
//! history sent to the model is derived from the log.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use syl_container::Container;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::client::{syl_tools, BlockDeltaEvent, CancelledError, SystemBlock};
use crate::api::compaction;
use crate::api::types::{ContentBlock, ImageSource, Message, MessageContent, StopReason, Usage};
use crate::checkpoint::Checkpoints;
use crate::control::{Pending, PermissionAnswer, Review};
pub use crate::permissions::Mode;
use crate::permissions::{Action, Policy};
use crate::config::AppConfig;
use crate::model::{ModelAdapter, ModelRequest};
use crate::session::{EntryKind, PromptBlock, SessionInfo, SessionLog};
use crate::shared::SharedState;
use crate::system_prompt::build_system_blocks;
use crate::tools::{self, ToolContext};
use crate::util::SpillDir;
use crate::workspace::gather_workspace_context;

/// What happened in the agent, in order. Serialised (for `--output json`)
/// as an object tagged with `"type"`: `{"type":"text_delta","text":"…"}`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A prompt was accepted and its turn began.
    TurnStarted,
    /// A model request is about to be sent (once per step of a tool chain).
    RequestStarted,
    TextDelta { text: String },
    ThinkingDelta { text: String },
    ToolCallStarted { id: String, name: String, input: serde_json::Value },
    ToolCallFinished { id: String, name: String, output: String, is_error: bool },
    /// A rule says to ask before this tool call (and the reviewer, if any,
    /// didn't approve it). The turn waits for `ApprovePermission`.
    /// `subject` is what the rule matched (for `run_bash`, the one command
    /// of several that needs asking); `note` is the reviewer's reason for
    /// blocking, if it did.
    PermissionRequested { id: String, name: String, input: serde_json::Value, subject: String, note: Option<String> },
    /// A call that wasn't simply allowed was decided: by a rule, the
    /// reviewer or the user.
    PermissionDecided { id: String, allowed: bool, by: String },
    /// The mode changed (it applies from the next turn).
    ModeChanged { mode: Mode },
    /// The workspace was snapshotted before this turn changed files.
    Checkpointed { commit: String },
    /// The workspace was restored to `commit`; `files` changed back.
    Restored { commit: String, files: usize },
    /// Token use of one model request, and the session total after it.
    Usage { request: Usage, session: Usage },
    /// History was compacted from `before` to `after` messages.
    Compacted { before: usize, after: usize },
    /// Status text: retry cool-downs, container recovery, log trouble.
    Notice { message: String },
    /// Inside a turn, the turn failed and `TurnEnded` follows. Outside a
    /// turn, a session command could not be carried out.
    Error { message: String },
    TurnEnded { reason: TurnEnd },
    /// The current session, sent when the loop starts and after every
    /// session command that succeeds.
    SessionChanged { info: SessionInfo },
    /// Steering reached the model, after the tool results of this turn.
    /// Every `Steer` is answered by exactly one of these three events.
    SteerDelivered { text: String },
    /// Steering arrived too late for its turn (or with no turn running):
    /// `count` steering messages run together as the next prompt, and a
    /// `TurnStarted` for it follows.
    SteerQueued { text: String, count: usize },
    /// Steering was dropped because its turn was cancelled or failed.
    SteerDropped { text: String, count: usize },
}

/// How a turn ended. Serialised as the stop reason (`"end_turn"`,
/// `"max_tokens"`, ...), `"cancelled"` or `"failed"`.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    /// The model stopped for this reason (never `ToolUse`).
    Completed(StopReason),
    Cancelled,
    /// An `Error` event with the cause came just before.
    Failed,
}

impl serde::Serialize for TurnEnd {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            TurnEnd::Completed(r) => s.serialize_str(r.as_str()),
            TurnEnd::Cancelled => s.serialize_str("cancelled"),
            TurnEnd::Failed => s.serialize_str("failed"),
        }
    }
}

/// What a client can ask of the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentCommand {
    /// Send a user message. Prompts sent during a turn run after it, in order.
    Prompt(String),
    /// A prompt with images (see `attachments`), placed before the text.
    PromptWithImages { text: String, images: Vec<ImageSource> },
    /// A message for the turn in progress: delivered after the tool calls
    /// running now finish, alongside their results, so the model sees it
    /// before its next step. If the turn ends first, it runs as the next
    /// prompt (a cancelled turn drops it, with a `Notice`). When idle, it
    /// is just a prompt.
    Steer(String),
    /// Stop the turn in progress. Ignored when idle.
    Cancel,
    /// Answer a `PermissionRequested` event.
    ApprovePermission { id: String, answer: PermissionAnswer },
    /// Plan (no file changes) or build. Applies from the next turn; the
    /// model is told.
    SetMode(Mode),
    /// Put the workspace back to a checkpoint (a commit from a `Checkpoint`
    /// entry or `Checkpointed` event). The current state is snapshotted
    /// first, so this can be undone too. Waits for a turn in progress.
    Restore { commit: String },
    /// Use this model from the next turn on.
    SetModel(String),
    /// Start a new, empty session. Like the session commands below, it
    /// waits for a turn in progress to end.
    NewSession,
    /// Switch to a saved session, by id.
    LoadSession(String),
    /// Continue in a new session that copies this one up to and including
    /// entry `at` (all of it when None).
    Fork { at: Option<String> },
    /// Name the current session.
    SetSessionName(String),
    /// Stop the loop: cancel any turn in progress (it still ends with
    /// `TurnEnded`, and everything up to the cancel is in the log), drop
    /// queued commands, and return. The loop also stops once every command
    /// sender is dropped, but a client that hands clones to signal handlers
    /// should send this instead.
    Shutdown,
}

/// Where a session keeps its files.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub project_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub workflow_dir: PathBuf,
    /// Session logs. None keeps sessions in memory only.
    pub sessions_dir: Option<PathBuf>,
    /// Where large tool output is saved, one subdirectory per session.
    pub spill: Option<SpillDir>,
    /// Snapshot the workspace before turns that change files (shadow git
    /// in `.syl/shadow.git`).
    pub checkpoints: bool,
    /// Rules saved by "always" answers.
    pub approvals: Option<PathBuf>,
}

impl SessionPaths {
    /// The standard layout under `<project>/.syl`, which the container
    /// mounts read-only at `/workspace/.syl`.
    pub fn for_project(project_dir: impl Into<PathBuf>) -> Self {
        let project_dir = project_dir.into();
        let syl_dir = project_dir.join(".syl");
        SessionPaths {
            memory_dir: syl_dir.join("memory"),
            workflow_dir: syl_dir.join("workflows"),
            sessions_dir: Some(syl_dir.join("sessions")),
            spill: Some(SpillDir {
                host: syl_dir.join("spill"),
                container: "/workspace/.syl/spill".to_string(),
            }),
            checkpoints: true,
            approvals: Some(syl_dir.join("approvals.toml")),
            project_dir,
        }
    }
}

/// Which session the loop starts in.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionStart {
    New,
    /// Load this session id; on failure the loop reports an `Error` and
    /// starts a new session.
    Resume(String),
}

/// Everything the loop runs with.
pub struct Agent {
    pub config: AppConfig,
    pub state: Arc<SharedState>,
    pub model: Arc<dyn ModelAdapter>,
    pub container: Arc<Container>,
    pub paths: SessionPaths,
    pub start: SessionStart,
}

/// The client's end of a running agent.
pub struct AgentHandle {
    pub commands: mpsc::UnboundedSender<AgentCommand>,
    pub events: mpsc::UnboundedReceiver<AgentEvent>,
    pub task: tokio::task::JoinHandle<()>,
}

/// Start the agent loop. It runs until `Shutdown` or until every command
/// sender is dropped.
pub fn spawn(agent: Agent) -> AgentHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run(agent, cmd_rx, ev_tx));
    AgentHandle { commands: cmd_tx, events: ev_rx, task }
}

type Events = mpsc::UnboundedSender<AgentEvent>;

/// Images kept in the history before older ones are dropped: the API
/// takes about 100 per request, and 32 MB in all.
const MAX_HISTORY_IMAGES: usize = 80;
const MAX_HISTORY_IMAGE_BYTES: usize = 24_000_000;

/// Whether an API error is about the images in the request (too large in
/// all, or one it can't read).
fn is_image_rejection(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("413") || e.contains("request_too_large") || (e.contains("400") && e.contains("image"))
}

/// What the loop knows about the size of the current context.
#[derive(Default)]
struct ContextState {
    /// Input tokens of the last request plus its output, and how many
    /// messages that covered (through the assistant reply).
    measured: Option<(usize, u64)>,
    /// Compact before the next request whatever the estimate says.
    force_compact: bool,
    /// The window size an overflow error reported, and for which model.
    learned_window: Option<(String, u64)>,
    /// History length just after the last compaction: compacting again
    /// before anything is added can't help.
    compacted_len: Option<usize>,
    /// A failed log write was already reported.
    log_error_reported: bool,
    /// A failed checkpoint was already reported.
    checkpoint_error_reported: bool,
    /// Nested git repositories were already reported.
    nested_repos_reported: bool,
}

fn open_session(dir: Option<&std::path::Path>, id: &str) -> anyhow::Result<SessionLog> {
    let dir = dir.ok_or_else(|| anyhow::anyhow!("sessions are not saved to disk"))?;
    SessionLog::open(dir, id)
}

async fn run(agent: Agent, mut commands: mpsc::UnboundedReceiver<AgentCommand>, events: Events) {
    let emit = |e: AgentEvent| {
        let _ = events.send(e);
    };
    let dir = agent.paths.sessions_dir.as_deref();
    let cwd = agent.paths.project_dir.as_path();

    let mut session = match &agent.start {
        SessionStart::New => SessionLog::create(dir, cwd),
        SessionStart::Resume(id) => open_session(dir, id).unwrap_or_else(|e| {
            emit(AgentEvent::Error { message: format!("could not resume session {id}: {e}") });
            SessionLog::create(dir, cwd)
        }),
    };
    let mut ctx = ContextState::default();
    adopt(&agent, &session);
    emit(AgentEvent::SessionChanged { info: session.info() });

    let mut deferred: VecDeque<AgentCommand> = VecDeque::new();
    let mut mode = Mode::Build;
    let pending: Pending = Default::default();
    // MCP servers start with the first turn and serve every session after.
    let mcp: tokio::sync::OnceCell<Arc<crate::mcp::McpHub>> = tokio::sync::OnceCell::new();
    if run_session_start_hooks(&agent, &session, &events, mode, &mut commands, &mut deferred).await {
        return;
    }
    loop {
        let cmd = match deferred.pop_front() {
            Some(c) => c,
            None => match commands.recv().await {
                Some(c) => c,
                None => break,
            },
        };
        let prompt = match cmd {
            AgentCommand::Shutdown => break,
            AgentCommand::Cancel | AgentCommand::ApprovePermission { .. } => continue,
            AgentCommand::SetMode(m) => {
                if m != mode {
                    mode = m;
                    record(&agent, &mut session, &mut ctx, &events, EntryKind::Note { text: mode_note(m).to_string() });
                }
                emit(AgentEvent::ModeChanged { mode });
                continue;
            }
            AgentCommand::Restore { commit } => {
                restore(&agent, &mut session, &mut ctx, &events, &commit).await;
                continue;
            }
            AgentCommand::SetModel(m) => {
                *agent.state.current_model.lock().unwrap() = m;
                continue;
            }
            AgentCommand::NewSession => {
                session = SessionLog::create(dir, cwd);
                switch_to(&agent, &session, &mut ctx, &events);
                if run_session_start_hooks(&agent, &session, &events, mode, &mut commands, &mut deferred).await {
                    break;
                }
                continue;
            }
            AgentCommand::LoadSession(id) if id == session.id() => {
                // Already open here (and locked: opening it again would fail).
                emit(AgentEvent::SessionChanged { info: session.info() });
                continue;
            }
            AgentCommand::LoadSession(id) => {
                match open_session(dir, &id) {
                    Ok(s) => {
                        session = s;
                        switch_to(&agent, &session, &mut ctx, &events);
                        if run_session_start_hooks(&agent, &session, &events, mode, &mut commands, &mut deferred).await {
                            break;
                        }
                    }
                    Err(e) => emit(AgentEvent::Error { message: format!("could not load session {id}: {e}") }),
                }
                continue;
            }
            AgentCommand::Fork { at } => {
                match session.fork(dir, cwd, at.as_deref()) {
                    Ok(s) => {
                        session = s;
                        switch_to(&agent, &session, &mut ctx, &events);
                        if run_session_start_hooks(&agent, &session, &events, mode, &mut commands, &mut deferred).await {
                            break;
                        }
                    }
                    Err(e) => emit(AgentEvent::Error { message: format!("could not fork: {e}") }),
                }
                continue;
            }
            AgentCommand::SetSessionName(name) => {
                record(&agent, &mut session, &mut ctx, &events, EntryKind::Name { name });
                emit(AgentEvent::SessionChanged { info: session.info() });
                continue;
            }
            AgentCommand::Steer(p) => {
                emit(AgentEvent::SteerQueued { text: p.clone(), count: 1 });
                Message::user_text(crate::commands::expand(&agent.paths.project_dir, &p).unwrap_or(p))
            }
            // `/name args` runs a custom command or a skill.
            AgentCommand::Prompt(p) => {
                Message::user_text(crate::commands::expand(&agent.paths.project_dir, &p).unwrap_or(p))
            }
            AgentCommand::PromptWithImages { text, images } => {
                let mut blocks: Vec<ContentBlock> =
                    images.into_iter().map(|source| ContentBlock::Image { source }).collect();
                let text = crate::commands::expand(&agent.paths.project_dir, &text).unwrap_or(text);
                // The API rejects empty text blocks: an image alone is fine.
                if !text.trim().is_empty() {
                    blocks.push(ContentBlock::Text { text, cache_control: None });
                }
                Message { role: "user".to_string(), content: MessageContent::Blocks(blocks) }
            }
        };

        emit(AgentEvent::TurnStarted);
        record(&agent, &mut session, &mut ctx, &events, EntryKind::Message { message: prompt });
        let steer: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

        let cancel = CancellationToken::new();
        let mut shutting_down = false;
        let mut dropped_prompts = 0usize;
        let reason = {
            let turn = run_turn(&agent, &mut session, &mut ctx, &events, &steer, &pending, mode, &mcp, cancel.clone());
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    reason = &mut turn => break reason,
                    cmd = commands.recv(), if !shutting_down => match cmd {
                        Some(AgentCommand::Shutdown) | None => {
                            shutting_down = true;
                            cancel.cancel();
                        }
                        Some(AgentCommand::Cancel) => {
                            cancel.cancel();
                            // Prompts queued behind this turn are cancelled too.
                            let before = deferred.len();
                            deferred.retain(|c| !matches!(c, AgentCommand::Prompt(_) | AgentCommand::PromptWithImages { .. }));
                            dropped_prompts += before - deferred.len();
                        }
                        // Takes effect next turn; this one already chose its
                        // model. Behind queued prompts it waits its turn.
                        Some(AgentCommand::SetModel(m)) if deferred.is_empty() => {
                            *agent.state.current_model.lock().unwrap() = m
                        }
                        Some(AgentCommand::ApprovePermission { id, answer }) => {
                            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(answer);
                            }
                        }
                        Some(AgentCommand::Steer(text)) => steer.lock().unwrap().push(crate::commands::expand(&agent.paths.project_dir, &text).unwrap_or(text)),
                        // Prompts and session commands wait for the turn.
                        Some(other) => deferred.push_back(other),
                    },
                }
            }
        };
        // turn_end hooks are part of the turn: Cancel stops them, and a
        // client waiting for TurnEnded (headless) waits for them too.
        if !shutting_down {
            let payload = serde_json::json!({ "event": "turn_end", "session_id": session.id(), "reason": reason });
            let ev = crate::hooks::HookEvent::TurnEnd;
            shutting_down |= run_event_hooks(&agent, &events, mode, ev, "turn_end", payload, &mut commands, &mut deferred).await;
        }
        let undelivered = std::mem::take(&mut *steer.lock().unwrap());
        emit(AgentEvent::TurnEnded { reason: reason.clone() });
        // Each prompt the cancel dropped still gets its turn's start and end.
        for _ in 0..dropped_prompts {
            emit(AgentEvent::TurnStarted);
            emit(AgentEvent::TurnEnded { reason: TurnEnd::Cancelled });
        }
        if !undelivered.is_empty() {
            let count = undelivered.len();
            let text = undelivered.join("\n\n");
            if matches!(reason, TurnEnd::Completed(_)) && !shutting_down {
                // The turn ended before it could hear it: it's the next prompt.
                emit(AgentEvent::SteerQueued { text: text.clone(), count });
                // Ahead of queued prompts, but after mode changes sent before
                // them: a switch to plan mode during the turn applies to it.
                let at = deferred
                    .iter()
                    .position(|c| matches!(c, AgentCommand::Prompt(_) | AgentCommand::PromptWithImages { .. } | AgentCommand::Steer(_)))
                    .unwrap_or(deferred.len());
                deferred.insert(at, AgentCommand::Prompt(text));
            } else {
                emit(AgentEvent::SteerDropped { text, count });
            }
        }
        if shutting_down {
            break;
        }
    }
}

/// Make `session` the one the shared state shows.
fn adopt(agent: &Agent, session: &SessionLog) {
    *agent.state.messages.lock().unwrap() = session.messages().to_vec();
    *agent.state.extra_tools.lock().unwrap() = Arc::new(session.history().tools.clone().unwrap_or_default());
    *agent.state.system_prompt.lock().unwrap() = session
        .history()
        .system
        .as_ref()
        .map(|blocks| blocks.iter().map(PromptBlock::to_system).collect());
}

fn switch_to(agent: &Agent, session: &SessionLog, ctx: &mut ContextState, events: &Events) {
    *ctx = ContextState::default();
    adopt(agent, session);
    let _ = events.send(AgentEvent::SessionChanged { info: session.info() });
}

/// Append to the log and mirror the change into the shared state. A failed
/// write is reported once per session; the entry still takes effect.
fn record(agent: &Agent, session: &mut SessionLog, ctx: &mut ContextState, events: &Events, kind: EntryKind) {
    let sync = match &kind {
        EntryKind::Message { .. } | EntryKind::ToolResult { .. } | EntryKind::Steer { .. } => Some(false),
        EntryKind::Compaction { .. } | EntryKind::Resumed | EntryKind::ImagesDropped { .. } => Some(true),
        _ => None,
    };
    if let Err(e) = session.append(kind) {
        tracing::error!("session log write failed: {e:#}");
        if !ctx.log_error_reported {
            ctx.log_error_reported = true;
            let path = session.path().map(|p| p.display().to_string()).unwrap_or_default();
            let _ = events.send(AgentEvent::Notice {
                message: format!("[Syl] Could not write the session log {path}: {e}. This session may not be resumable."),
            });
        }
    }
    let Some(full) = sync else { return };
    let src = session.messages();
    let mut mirror = agent.state.messages.lock().unwrap();
    if !full && mirror.len() == src.len() && !src.is_empty() {
        *mirror.last_mut().expect("non-empty") = src.last().expect("non-empty").clone();
    } else if !full && mirror.len() + 1 == src.len() {
        mirror.push(src.last().expect("non-empty").clone());
    } else {
        *mirror = src.to_vec();
    }
}

/// The system prompt for this conversation: built while history has no
/// assistant turn yet, then frozen (see `SharedState::system_prompt`).
/// Each prompt that differs from the one last logged is logged.
fn system_prompt(
    agent: &Agent,
    session: &mut SessionLog,
    ctx: &mut ContextState,
    events: &Events,
    mcp: &crate::mcp::McpHub,
) -> Vec<SystemBlock> {
    // Fresh until the first assistant turn is logged (compaction can leave
    // a history with none, but the conversation isn't new).
    let fresh_conversation = !session.entries().iter().any(|e| {
        matches!(&e.kind, EntryKind::Message { message } if message.role == "assistant")
    });
    let current = agent.state.system_prompt.lock().unwrap().clone();
    if let (false, Some(blocks)) = (fresh_conversation, current) {
        return blocks;
    }
    // Skills and MCP tools are fixed with the prompt, for the conversation.
    let skills = crate::commands::list_skills(&agent.paths.project_dir);
    let mut workspace = gather_workspace_context(&agent.paths.project_dir);
    let res = agent.container.resources();
    workspace.push_str(&format!(
        "Container: {} memory, {} CPUs. CARGO_BUILD_JOBS, MAKEFLAGS and CMAKE_BUILD_PARALLEL_LEVEL \
         are set to {}, which fit the memory; going above that can get processes killed.\n",
        res.memory_text(),
        res.cpus,
        res.jobs_text()
    ));
    workspace.push_str(&crate::commands::skills_prompt(&skills).unwrap_or_default());
    workspace.push_str(&crate::mcp::instructions_prompt(mcp).unwrap_or_default());
    let mut tools = mcp.definitions.clone();
    if !skills.is_empty() {
        tools.push(tools::skill_tool());
    }
    if session.history().tools.as_ref().map_or(!tools.is_empty(), |t| *t != tools) {
        record(agent, session, ctx, events, EntryKind::ToolSet { tools: tools.clone() });
    }
    *agent.state.extra_tools.lock().unwrap() = Arc::new(tools);
    let instructions = crate::system_prompt::load_project_instructions(&agent.paths.project_dir);
    let blocks: Vec<SystemBlock> = build_system_blocks(&workspace, &agent.paths.memory_dir, instructions.as_deref())
        .into_iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, cache_control } => Some(if cache_control.is_some() {
                SystemBlock::cached(text)
            } else {
                SystemBlock::text(text)
            }),
            _ => None,
        })
        .collect();
    let logged: Vec<PromptBlock> = blocks.iter().map(PromptBlock::from_system).collect();
    if session.history().system.as_ref() != Some(&logged) {
        record(agent, session, ctx, events, EntryKind::SystemPrompt { blocks: logged });
    }
    *agent.state.system_prompt.lock().unwrap() = Some(blocks.clone());
    blocks
}

/// The context window for `model`: what an overflow error said for this
/// model, then config, then the Models API, then a per-model default.
fn context_window(agent: &Agent, config: &AppConfig, ctx: &ContextState) -> u64 {
    let learned = ctx.learned_window.as_ref().filter(|(m, _)| *m == config.model).map(|(_, w)| *w);
    learned
        .or(config.context_window)
        .or_else(|| {
            agent
                .state
                .available_models
                .lock()
                .unwrap()
                .iter()
                .find(|m| m.id == config.model)
                .and_then(|m| m.max_input_tokens)
        })
        .unwrap_or_else(|| compaction::default_context_window(&config.model))
}

/// Tokens the next request will send: the last measured request plus an
/// estimate of what was added since, or an estimate of everything.
fn context_estimate(ctx: &ContextState, messages: &[Message], system: &[SystemBlock], extra_tools: &[serde_json::Value]) -> u64 {
    match ctx.measured {
        Some((n, tokens)) if n <= messages.len() => {
            tokens + messages[n..].iter().map(compaction::estimate_tokens).sum::<u64>()
        }
        _ => {
            messages.iter().map(compaction::estimate_tokens).sum::<u64>()
                + system.iter().map(|b| compaction::estimate_text_tokens(&b.text)).sum::<u64>()
                + compaction::estimate_text_tokens(&syl_tools().to_string())
                + extra_tools.iter().map(|t| compaction::estimate_text_tokens(&t.to_string())).sum::<u64>()
        }
    }
}

/// Summarize the older part of the history and log a compaction entry.
/// Returns false, changing nothing, when there is no safe place to cut, or
/// on cancel.
#[allow(clippy::too_many_arguments)]
async fn compact(
    agent: &Agent,
    session: &mut SessionLog,
    ctx: &mut ContextState,
    events: &Events,
    config: &AppConfig,
    window: u64,
    tokens_before: u64,
    cancel: CancellationToken,
) -> bool {
    let msgs = session.messages().to_vec();
    let keep_tokens = compaction::effective_keep_tokens(config.compact_keep_tokens, window, config.compact_buffer);
    let window_start = compaction::keep_window_start(&msgs, keep_tokens);
    let Some(boundary) = compaction::choose_keep_boundary(&msgs, window_start) else {
        tracing::warn!(messages = msgs.len(), "compaction: no safe boundary; skipping");
        return false;
    };
    let keep = boundary.index();
    let Some(first_kept) = session.history().message_start(keep).map(str::to_string) else {
        return false;
    };
    tracing::info!(old = keep, keep = msgs.len() - keep, tokens_before, "compacting history");

    let summary = compaction::summarize_messages(agent.model.as_ref(), config, &msgs[..keep], cancel.clone()).await;
    if cancel.is_cancelled() {
        return false;
    }
    if summary.is_none() {
        let _ = events.send(AgentEvent::Notice {
            message: "[Syl] Could not summarize older turns; they were dropped from the model's \
                      context (they are still in the session log)."
                .to_string(),
        });
    }
    record(agent, session, ctx, events, EntryKind::Compaction {
        summary,
        first_kept,
        tokens_before: Some(tokens_before),
    });
    ctx.measured = None;
    ctx.compacted_len = Some(session.messages().len());
    let _ = events.send(AgentEvent::Compacted { before: msgs.len(), after: session.messages().len() });
    true
}

/// One user turn: model calls and tool calls until the model stops.
#[allow(clippy::too_many_arguments)]
async fn run_turn(
    agent: &Agent,
    session: &mut SessionLog,
    ctx: &mut ContextState,
    events: &Events,
    steer: &std::sync::Mutex<Vec<String>>,
    pending: &Pending,
    mode: Mode,
    mcp_cell: &tokio::sync::OnceCell<Arc<crate::mcp::McpHub>>,
    cancel: CancellationToken,
) -> TurnEnd {
    let emit = |e: AgentEvent| {
        let _ = events.send(e);
    };

    // The model is fixed for the whole turn.
    let mut config = {
        let mut c = agent.config.clone();
        let model = agent.state.current_model.lock().unwrap().clone();
        if !model.is_empty() {
            c.model = model;
        }
        c
    };
    // Started on the first turn. Cancelling leaves it unstarted, so the next
    // turn tries again.
    let started = mcp_cell
        .get_or_try_init(|| async {
            if agent.config.mcp.is_empty() {
                return Ok(Arc::new(crate::mcp::McpHub::default()));
            }
            emit(AgentEvent::Notice { message: format!("[Syl] Starting {} MCP server(s)...", agent.config.mcp.len()) });
            let http = reqwest::Client::new();
            let start = crate::mcp::McpHub::start(
                &agent.config.mcp,
                &agent.container,
                &agent.paths.project_dir,
                &http,
                std::time::Duration::from_secs(60),
            );
            let (hub, problems) = tokio::select! {
                r = start => r,
                _ = cancel.cancelled() => return Err(()),
            };
            for p in problems {
                emit(AgentEvent::Notice { message: format!("[Syl] {p}") });
            }
            Ok(Arc::new(hub))
        })
        .await;
    let Ok(mcp) = started.cloned() else { return TurnEnd::Cancelled };
    if session.history().model.as_deref() != Some(config.model.as_str()) {
        record(agent, session, ctx, events, EntryKind::ModelChange { model: config.model.clone() });
    }
    let spill = agent.paths.spill.as_ref().map(|s| SpillDir {
        host: s.host.join(session.id()),
        container: format!("{}/{}", s.container.trim_end_matches('/'), session.id()),
    });

    // The permission rules for this turn: config, then saved "always"
    // answers, then the mode's. "Always" answers during the turn add to it.
    let mut approvals = match agent.paths.approvals.as_deref().map(crate::permissions::load_approvals) {
        Some(Ok(a)) => a,
        Some(Err(e)) => {
            emit(AgentEvent::Notice {
                message: format!("[Syl] Saved approvals can't be read, so they don't apply (and none will be saved): {e}"),
            });
            Vec::new()
        }
        None => Vec::new(),
    };
    // In plan mode the turn isn't checkpointed: each run_bash is snapshotted
    // and put back on its own, and /undo should find the last build turn.
    let mut checkpointed = mode == Mode::Plan;

    let mut overflow_retries = 0;
    let mut images_dropped = false;
    // The error of a request rejected as too long, while it is retried.
    let mut overflow: Option<String> = None;
    loop {
        emit(AgentEvent::RequestStarted);
        let system = system_prompt(agent, session, ctx, events, &mcp);
        config.extra_tools = agent.state.extra_tools.lock().unwrap().clone();

        // Compact before every model call, not just between user messages:
        // a long tool chain inside one turn can otherwise grow without bound.
        // Mid-chain (the last message holds tool results) compaction strips
        // thinking from the turn in progress, so it waits until the context
        // is twice as close to full.
        let window = context_window(agent, &config, ctx);
        let estimate = context_estimate(ctx, session.messages(), &system, &config.extra_tools)
            + crate::api::client::request_max_tokens(&config);
        let mid_chain = session.messages().last().is_some_and(|m| {
            matches!(&m.content, crate::api::types::MessageContent::Blocks(b)
                if b.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })))
        });
        let buffer = if mid_chain { config.compact_buffer / 2.0 } else { config.compact_buffer };
        let unchanged = ctx.compacted_len == Some(session.messages().len());
        let due = !unchanged && compaction::needs_compaction(estimate, window, buffer);
        if ctx.force_compact || due {
            let forced = std::mem::take(&mut ctx.force_compact);
            let compacted = compact(agent, session, ctx, events, &config, window, estimate, cancel.clone()).await;
            if let (false, Some(err)) = (compacted || cancel.is_cancelled(), overflow.take().filter(|_| forced)) {
                // Too long, and nothing left to compact: sending it again
                // would fail the same way.
                emit(AgentEvent::Error { message: err });
                return TurnEnd::Failed;
            }
        }
        if cancel.is_cancelled() {
            return TurnEnd::Cancelled;
        }
        // The API caps a request at 32 MB and ~100 images: keep well under.
        let (images, image_bytes) = crate::session::image_load(session.messages());
        if images > MAX_HISTORY_IMAGES || image_bytes > MAX_HISTORY_IMAGE_BYTES {
            record(agent, session, ctx, events, EntryKind::ImagesDropped { all: false });
            emit(AgentEvent::Notice {
                message: "[Syl] Older images were removed from the context to keep requests under the API's size limit.".to_string(),
            });
        }

        let messages = session.messages().to_vec();
        let on_notice = |msg: &str| emit(AgentEvent::Notice { message: msg.to_string() });
        let on_delta = |delta: BlockDeltaEvent| match delta {
            BlockDeltaEvent::TextDelta { text } => emit(AgentEvent::TextDelta { text }),
            BlockDeltaEvent::ThinkingDelta { text } => emit(AgentEvent::ThinkingDelta { text }),
            BlockDeltaEvent::InputJsonDelta { .. } => {}
        };
        let req = ModelRequest { config: &config, messages: &messages, system: &system };
        let turn = match agent.model.complete(req, cancel.clone(), &on_notice, &on_delta).await {
            Ok(t) => t,
            Err(e) if e.downcast_ref::<CancelledError>().is_some() => {
                tracing::info!("turn cancelled by user");
                return TurnEnd::Cancelled;
            }
            Err(e) if compaction::is_context_overflow(&e.to_string()) && overflow_retries < 2 => {
                // The estimate was off: compact now and send again.
                overflow_retries += 1;
                let text = e.to_string();
                if let Some(limit) = compaction::overflow_limit(&text) {
                    ctx.learned_window = Some((config.model.clone(), limit));
                }
                tracing::warn!("context overflow, compacting and retrying: {text}");
                ctx.force_compact = true;
                ctx.compacted_len = None;
                ctx.measured = None;
                overflow = Some(text);
                continue;
            }
            Err(e) if !images_dropped && is_image_rejection(&e.to_string()) && crate::session::image_load(session.messages()).0 > 0 => {
                // An image the API won't take would fail every later
                // request too: take them all out and try once more.
                images_dropped = true;
                tracing::warn!("request rejected over images, retrying without them: {e}");
                record(agent, session, ctx, events, EntryKind::ImagesDropped { all: true });
                emit(AgentEvent::Notice {
                    message: format!("[Syl] The API rejected a request with images ({}); retrying without them.", crate::session::one_line(&e.to_string(), 160)),
                });
                ctx.measured = None;
                continue;
            }
            Err(e) => {
                emit(AgentEvent::Error { message: e.to_string() });
                return TurnEnd::Failed;
            }
        };

        let session_usage = {
            let mut tok = agent.state.session_tokens.lock().unwrap();
            tok.input_tokens += turn.usage.input_tokens;
            tok.output_tokens += turn.usage.output_tokens;
            tok.cache_read_tokens += turn.usage.cache_read_tokens;
            tok.cache_write_tokens += turn.usage.cache_write_tokens;
            tok.clone()
        };
        emit(AgentEvent::Usage { request: turn.usage.clone(), session: session_usage });

        // A tool_use the model did not finish (stop_reason max_tokens etc.)
        // gets no tool_result, and a tool_use without one makes every later
        // request fail with a 400, so only a tool_use turn keeps them.
        record(agent, session, ctx, events, EntryKind::Message {
            message: Message::assistant_blocks(compaction::drop_unanswered_tool_uses(
                &turn.content,
                &turn.stop_reason,
            )),
        });
        let u = &turn.usage;
        ctx.measured = Some((
            session.messages().len(),
            u.input_tokens + u.cache_read_tokens + u.cache_write_tokens + u.output_tokens,
        ));

        if turn.stop_reason == StopReason::ContextWindowExceeded {
            ctx.force_compact = true;
        }
        if turn.stop_reason != StopReason::ToolUse {
            return TurnEnd::Completed(turn.stop_reason);
        }

        let calls: Vec<(String, String, serde_json::Value)> = turn
            .content
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some((id, name, input)),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            return TurnEnd::Completed(StopReason::ToolUse);
        }

        // Recovering a dead container can take minutes of blocking podman
        // calls: run them off the async runtime.
        let container = agent.container.clone();
        let recovered = tokio::task::spawn_blocking(move || {
            let notes = std::sync::Mutex::new(Vec::new());
            let r = container.ensure(&|m| notes.lock().unwrap().push(m.to_string()));
            (r, notes.into_inner().unwrap_or_default())
        })
        .await;
        match recovered {
            Ok((r, notes)) => {
                for n in notes {
                    emit(AgentEvent::Notice { message: n });
                }
                if let Err(e) = r {
                    emit(AgentEvent::Notice { message: format!("[Syl] Container recovery failed: {e}") });
                }
            }
            Err(e) => emit(AgentEvent::Notice { message: format!("[Syl] Container recovery failed: {e}") }),
        }

        let tool_ctx = ToolContext {
            config: &config,
            model: agent.model.as_ref(),
            container: &agent.container,
            memory_dir: &agent.paths.memory_dir,
            workflow_dir: &agent.paths.workflow_dir,
            spill: spill.as_ref(),
            project_dir: &agent.paths.project_dir,
            mcp: Some(&mcp),
        };
        let plan_denied = if mode == Mode::Plan { mcp.mutating() } else { Vec::new() };
        // Consecutive read-only calls run together; anything else runs alone.
        let mut i = 0;
        while i < calls.len() {
            let j = if tools::is_read_only(&calls[i].1) {
                calls[i..].iter().position(|c| !tools::is_read_only(&c.1)).map_or(calls.len(), |n| i + n)
            } else {
                i + 1
            };
            let group = &calls[i..j];
            i = j;
            // Every tool_use must get a tool_result, even after a cancel.
            if cancel.is_cancelled() {
                for (id, _, _) in group {
                    record(agent, session, ctx, events, EntryKind::ToolResult {
                        tool_use_id: id.clone(),
                        content: "[Tool error: cancelled by user]".to_string(),
                        is_error: true,
                    });
                }
                continue;
            }
            // Permissions first, one call at a time (asking is sequential).
            let mut results: Vec<Option<(String, bool)>> = vec![None; group.len()];
            for (k, (id, name, input)) in group.iter().enumerate() {
                let gate = Gate { agent, config: &config, pending, mode, cancel: &cancel, plan_denied: &plan_denied };
                if let Err(denied) = gate.authorize(session, ctx, events, &mut approvals, id, name, input).await {
                    results[k] = Some((denied, true));
                }
            }
            // A checkpoint before the first call of the turn that can change files.
            let will_change = group
                .iter()
                .zip(&results)
                .any(|((_, name, _), r)| {
                    r.is_none() && may_write(agent, &mcp, name)
                });
            if will_change && !checkpointed && agent.paths.checkpoints && !cancel.is_cancelled() {
                checkpointed = true;
                let label = crate::session::user_prompts(session.entries())
                    .last()
                    .map(|p| crate::session::one_line(&p.text, 72))
                    .unwrap_or_default();
                checkpoint_now(agent, session, ctx, events, &format!("before: {label}")).await;
            }
            let refused: Vec<bool> = results.iter().map(Option::is_some).collect();
            for ((id, name, input), refused) in group.iter().zip(&refused) {
                if !refused {
                    emit(AgentEvent::ToolCallStarted { id: id.clone(), name: name.clone(), input: input.clone() });
                }
            }
            let runnable: Vec<usize> = (0..group.len()).filter(|&k| results[k].is_none()).collect();
            let outputs = futures_util::future::join_all(runnable.iter().map(|&k| {
                let (id, name, input) = &group[k];
                execute_call(agent, &tool_ctx, mode, session.id(), id, name, input, cancel.clone())
            }))
            .await;
            for (k, (output, is_error, put_back)) in runnable.into_iter().zip(outputs) {
                if let Some((commit, safety)) = put_back {
                    // The files as they were before the put-back stay reachable.
                    record(agent, session, ctx, events, EntryKind::Restored { commit, safety });
                }
                results[k] = Some((output, is_error));
            }
            for (((id, name, input), result), refused) in group.iter().zip(results).zip(refused) {
                let (output, is_error) = result.expect("every call has a result");
                if refused {
                    // It never ran; clients still see a started/finished pair.
                    emit(AgentEvent::ToolCallStarted { id: id.clone(), name: name.clone(), input: input.clone() });
                }
                emit(AgentEvent::ToolCallFinished {
                    id: id.clone(),
                    name: name.clone(),
                    output: output.clone(),
                    is_error,
                });
                // Logged as soon as it exists, so a crash keeps finished work.
                record(agent, session, ctx, events, EntryKind::ToolResult {
                    tool_use_id: id.clone(),
                    content: output,
                    is_error,
                });
            }
        }

        // Steering typed during these calls goes in with their results.
        if !cancel.is_cancelled() {
            let pending = std::mem::take(&mut *steer.lock().unwrap());
            for text in pending {
                record(agent, session, ctx, events, EntryKind::Steer { text: text.clone() });
                emit(AgentEvent::SteerDelivered { text });
            }
        }

        if cancel.is_cancelled() {
            return TurnEnd::Cancelled;
        }
    }
}

/// `session_start` hooks; a failure is a notice.
/// Returns true when a Shutdown came while they ran.
async fn run_session_start_hooks(
    agent: &Agent,
    session: &SessionLog,
    events: &Events,
    mode: Mode,
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
    deferred: &mut VecDeque<AgentCommand>,
) -> bool {
    let payload = serde_json::json!({ "event": "session_start", "session_id": session.id(), "resumed": session.info().resumed });
    let ev = crate::hooks::HookEvent::SessionStart;
    run_event_hooks(agent, events, mode, ev, "session_start", payload, commands, deferred).await
}

/// Await `fut` while taking commands: Cancel and Shutdown cancel it (the
/// bool says a Shutdown came), anything else waits its turn.
async fn listening<F: std::future::Future>(
    fut: F,
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
    deferred: &mut VecDeque<AgentCommand>,
    cancel: &CancellationToken,
) -> (F::Output, bool) {
    tokio::pin!(fut);
    let mut shutdown = false;
    loop {
        tokio::select! {
            out = &mut fut => return (out, shutdown),
            cmd = commands.recv(), if !shutdown => match cmd {
                Some(AgentCommand::Shutdown) | None => {
                    shutdown = true;
                    cancel.cancel();
                }
                Some(AgentCommand::Cancel) => cancel.cancel(),
                Some(other) => deferred.push_back(other),
            },
        }
    }
}

/// One lock per project, shared by the sessions in this process: plan mode's
/// snapshot-run-put-back holds it exclusively; file-changing calls share it.
fn project_lock(dir: &std::path::Path) -> Arc<tokio::sync::RwLock<()>> {
    static LOCKS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<tokio::sync::RwLock<()>>>>> =
        std::sync::LazyLock::new(Default::default);
    let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    LOCKS.lock().unwrap().entry(key).or_default().clone()
}

/// Tools that can change files in the workspace: a turn is checkpointed
/// before the first of these runs.
const CHANGES_FILES: &[&str] = &["run_bash", "edit_file", "write_file"];

fn mode_note(mode: Mode) -> &'static str {
    match mode {
        Mode::Plan => {
            "[Syl is now in plan mode: edit_file and write_file are turned off, and anything run_bash \
             changes in the workspace is put back. Investigate, then propose a plan; the user will \
             switch to build mode to carry it out.]"
        }
        Mode::Build => "[Syl is back in build mode: file changes are allowed again.]",
    }
}

/// Snapshot the workspace and log it. A failure is reported once per
/// session and otherwise ignored: checkpoints are a safety net, not a gate.
async fn checkpoint_now(
    agent: &Agent,
    session: &mut SessionLog,
    ctx: &mut ContextState,
    events: &Events,
    label: &str,
) -> Option<String> {
    let cp = Checkpoints::for_project(&agent.paths.project_dir);
    let msg = label.to_string();
    let warn_nested = !ctx.nested_repos_reported;
    match tokio::task::spawn_blocking(move || cp.snapshot(&msg).map(|c| (c, if warn_nested { cp.nested_repos() } else { Vec::new() })))
        .await
        .map(|r| r.map(|(c, nested)| {
            if !nested.is_empty() {
                let _ = events.send(AgentEvent::Notice {
                    message: format!(
                        "[Syl] These directories are git repositories of their own, so checkpoints hold only \
                         their commit, not their files: {}",
                        nested.join(", ")
                    ),
                });
            }
            c
        })) {
        Ok(Ok(commit)) => {
            ctx.nested_repos_reported = true;
            record(agent, session, ctx, events, EntryKind::Checkpoint { commit: commit.clone(), label: label.to_string() });
            let _ = events.send(AgentEvent::Checkpointed { commit: commit.clone() });
            Some(commit)
        }
        Ok(Err(e)) => {
            report_checkpoint_error(ctx, events, &e.to_string());
            None
        }
        Err(e) => {
            report_checkpoint_error(ctx, events, &e.to_string());
            None
        }
    }
}

fn report_checkpoint_error(ctx: &mut ContextState, events: &Events, e: &str) {
    tracing::warn!("checkpoint failed: {e}");
    if !ctx.checkpoint_error_reported {
        ctx.checkpoint_error_reported = true;
        let _ = events.send(AgentEvent::Notice {
            message: format!("[Syl] Could not snapshot the workspace, so this turn can't be undone: {e}"),
        });
    }
}

/// `AgentCommand::Restore`: put the workspace back and tell the model.
async fn restore(agent: &Agent, session: &mut SessionLog, ctx: &mut ContextState, events: &Events, commit: &str) {
    let cp = Checkpoints::for_project(&agent.paths.project_dir);
    let c = commit.to_string();
    let done = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let changes = cp.preview(&c)?;
        let safety = cp.restore(&c, "requested")?;
        Ok((changes, safety))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|r| r);
    match done {
        Ok((changes, safety)) => {
            record(agent, session, ctx, events, EntryKind::Restored { commit: commit.to_string(), safety });
            let list: Vec<&str> = changes.iter().take(20).map(|c| c.path.as_str()).collect();
            let more = changes.len().saturating_sub(20);
            let list = if more > 0 { format!("{} and {more} more", list.join(", ")) } else { list.join(", ") };
            let text = if changes.is_empty() {
                format!("[The user restored the workspace to checkpoint {}; nothing had changed since.]", crate::checkpoint::short(commit))
            } else {
                format!(
                    "[The user restored the workspace to checkpoint {}: {} files went back ({list}). \
                     Changes made after that checkpoint, by you or the user, are undone (files that \
                     .gitignore hides, and the git repository itself, are as they were). Read files \
                     again before editing them.]",
                    crate::checkpoint::short(commit),
                    changes.len()
                )
            };
            record(agent, session, ctx, events, EntryKind::Note { text });
            let _ = events.send(AgentEvent::Restored { commit: commit.to_string(), files: changes.len() });
        }
        Err(e) => {
            let _ = events.send(AgentEvent::Error { message: format!("could not restore {}: {e}", crate::checkpoint::short(commit)) });
        }
    }
}

/// Whether a call may change files: a file tool, an MCP tool that doesn't
/// say it only reads, or any call a tool hook runs around.
fn may_write(agent: &Agent, mcp: &crate::mcp::McpHub, name: &str) -> bool {
    CHANGES_FILES.contains(&name)
        || (mcp.has(name) && !mcp.is_read_only(name))
        || has_tool_hooks(agent, name)
}

fn has_tool_hooks(agent: &Agent, name: &str) -> bool {
    use crate::hooks::{matching, HookEvent};
    !matching(&agent.config.hooks, HookEvent::PreTool, Some(name)).is_empty()
        || !matching(&agent.config.hooks, HookEvent::PostTool, Some(name)).is_empty()
}

/// Run one tool call, with its hooks. In plan mode a call that could write
/// (`run_bash`, any MCP tool, any call with hooks) runs between two
/// snapshots, and anything it or its hooks changed in the workspace is put
/// back; it is refused when no snapshot can be taken, since nothing could
/// be put back. Returns the output, and for a put-back the checkpoint and
/// the safety snapshot.
#[allow(clippy::too_many_arguments)]
async fn execute_call(
    agent: &Agent,
    tool_ctx: &ToolContext<'_>,
    mode: Mode,
    session_id: &str,
    id: &str,
    name: &str,
    input: &serde_json::Value,
    cancel: CancellationToken,
) -> (String, bool, Option<(String, String)>) {
    let lock = project_lock(&agent.paths.project_dir);
    let is_mcp = tool_ctx.mcp.is_some_and(|m| m.has(name));
    let hooked = has_tool_hooks(agent, name);
    if !(mode == Mode::Plan && (name == "run_bash" || is_mcp || hooked)) {
        // Calls that may change files wait while a plan-mode command (in
        // another session on this project) is between its snapshot and its
        // put-back.
        let writes = CHANGES_FILES.contains(&name) || is_mcp || hooked;
        let _shared = if writes { Some(lock.read().await) } else { None };
        let (o, e) = run_hooked(agent, tool_ctx, session_id, id, name, input, &cancel).await;
        return (o, e, None);
    }
    let run = run_hooked(agent, tool_ctx, session_id, id, name, input, &cancel);
    let ((output, is_error), put_back) = match in_snapshot(agent, run).await {
        Ok(r) => r,
        Err(why) => {
            return (
                format!("[Plan mode: {name} is unavailable because the workspace can't be snapshotted ({why}), so changes couldn't be put back.]"),
                true,
                None,
            )
        }
    };
    if let Some(e) = put_back.error {
        return (format!("{output}\n\n[Plan mode: could not check for file changes: {e}]"), true, None);
    }
    let mut notes = Vec::new();
    if !put_back.changed.is_empty() {
        notes.push(format!(
            "[Plan mode: this call changed files in the workspace, so they were put back ({}). In plan \
             mode, investigate and propose changes instead of making them.]",
            put_back.changed.join(", ")
        ));
    }
    if !put_back.lost.is_empty() {
        notes.push(format!("[Plan mode: these ignored paths were removed and can't be put back: {}.]", put_back.lost.join(", ")));
    }
    if notes.is_empty() {
        (output, is_error, None)
    } else {
        (format!("{output}\n\n{}", notes.join("\n")), true, put_back.safety)
    }
}

/// What a plan-mode put-back did.
#[derive(Default)]
struct PutBack {
    /// Paths that were changed and put back (at most 10).
    changed: Vec<String>,
    /// Ignored paths that were removed and can't be put back (at most 10).
    lost: Vec<String>,
    /// The snapshot before, and the safety snapshot of what was undone.
    safety: Option<(String, String)>,
    /// The check itself failed.
    error: Option<String>,
}

/// Run `work` between two snapshots, holding the project lock exclusively
/// (so the put-back can only undo what `work` did), and put back whatever
/// it changed in the workspace. Err (and `work` not run) when no snapshot
/// can be taken.
async fn in_snapshot<F: std::future::Future>(agent: &Agent, work: F) -> Result<(F::Output, PutBack), String> {
    let lock = project_lock(&agent.paths.project_dir);
    let _exclusive = lock.write().await;
    if !agent.paths.checkpoints {
        return Err("checkpoints are off".to_string());
    }
    let cp = Checkpoints::for_project(&agent.paths.project_dir);
    let snap = cp.clone();
    let before = tokio::task::spawn_blocking(move || -> anyhow::Result<(String, Vec<String>)> {
        Ok((snap.snapshot("plan mode: before a call")?, snap.ignored_paths().unwrap_or_default()))
    })
    .await;
    let (before, ignored_before) = match before {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(e) => return Err(e.to_string()),
    };
    let out = work.await;
    let check = before.clone();
    let put_back = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<crate::checkpoint::Change>, Option<String>, Vec<String>)> {
        let changes = cp.preview(&check)?;
        let safety = if changes.is_empty() { None } else { Some(cp.restore(&check, "plan mode")?) };
        let now: std::collections::HashSet<String> = cp.ignored_paths().unwrap_or_default().into_iter().collect();
        let lost = ignored_before.into_iter().filter(|p| !now.contains(p)).collect();
        Ok((changes, safety, lost))
    })
    .await;
    let result = match put_back {
        Ok(Ok((changes, safety, lost))) => PutBack {
            changed: changes.iter().take(10).map(|c| c.path.clone()).collect(),
            lost: lost.into_iter().take(10).collect(),
            safety: safety.map(|s| (before, s)),
            error: None,
        },
        Ok(Err(e)) => PutBack { error: Some(e.to_string()), ..Default::default() },
        Err(e) => PutBack { error: Some(e.to_string()), ..Default::default() },
    };
    Ok((out, result))
}

/// Run the hooks for a session event (`session_start`, `turn_end`) while
/// taking commands (see [`listening`]). In build mode they share the
/// project lock like any write; in plan mode they run in a snapshot and
/// what they changed is put back. Returns true when a Shutdown came.
#[allow(clippy::too_many_arguments)]
async fn run_event_hooks(
    agent: &Agent,
    events: &Events,
    mode: Mode,
    event: crate::hooks::HookEvent,
    label: &str,
    payload: serde_json::Value,
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
    deferred: &mut VecDeque<AgentCommand>,
) -> bool {
    use crate::hooks::{run_all, HookOutcome};
    if crate::hooks::matching(&agent.config.hooks, event, None).is_empty() {
        return false;
    }
    let notice = |m: String| {
        let _ = events.send(AgentEvent::Notice { message: m });
    };
    let cancel = CancellationToken::new();
    let hooks = run_all(&agent.container, &agent.config.hooks, event, None, &payload, &cancel);
    let (outcome, shutdown) = if mode == Mode::Plan {
        let (result, shutdown) = listening(in_snapshot(agent, hooks), commands, deferred, &cancel).await;
        match result {
            Err(why) => {
                notice(format!("[Syl] Plan mode: the {label} hooks didn't run, because the workspace can't be snapshotted ({why})."));
                (HookOutcome::Ok, shutdown)
            }
            Ok((outcome, put_back)) => {
                if let Some(e) = put_back.error {
                    notice(format!("[Syl] Plan mode: could not check what the {label} hooks changed: {e}"));
                } else if !put_back.changed.is_empty() {
                    notice(format!(
                        "[Syl] Plan mode: the {label} hooks changed files, so they were put back ({}).",
                        put_back.changed.join(", ")
                    ));
                }
                (outcome, shutdown)
            }
        }
    } else {
        let lock = project_lock(&agent.paths.project_dir);
        let shared = async {
            let _shared = lock.read().await;
            hooks.await
        };
        listening(shared, commands, deferred, &cancel).await
    };
    if let HookOutcome::Failed(why) = outcome {
        if !cancel.is_cancelled() {
            notice(format!("[Syl] {label} hook: {why}"));
        }
    }
    shutdown
}

/// The pre_tool hooks, the call, then the post_tool hooks.
#[allow(clippy::too_many_arguments)]
async fn run_hooked(
    agent: &Agent,
    tool_ctx: &ToolContext<'_>,
    session_id: &str,
    id: &str,
    name: &str,
    input: &serde_json::Value,
    cancel: &CancellationToken,
) -> (String, bool) {
    use crate::hooks::{matching, run_all, HookEvent, HookOutcome};
    let hooks = &agent.config.hooks;
    if !matching(hooks, HookEvent::PreTool, Some(name)).is_empty() {
        let payload = serde_json::json!({
            "event": "pre_tool", "session_id": session_id, "tool": name, "tool_use_id": id, "input": input,
        });
        if let HookOutcome::Failed(why) = run_all(&agent.container, hooks, HookEvent::PreTool, Some(name), &payload, cancel).await {
            if cancel.is_cancelled() {
                return ("[Cancelled]".to_string(), true);
            }
            return (format!("[Blocked by a pre_tool hook: {why}]"), true);
        }
    }
    let (output, is_error) = tools::execute(name, id, input, tool_ctx, cancel.clone()).await;
    if !matching(hooks, HookEvent::PostTool, Some(name)).is_empty() && !cancel.is_cancelled() {
        let payload = serde_json::json!({
            "event": "post_tool", "session_id": session_id, "tool": name, "tool_use_id": id, "input": input,
            "output": output, "is_error": is_error,
        });
        if let HookOutcome::Failed(why) = run_all(&agent.container, hooks, HookEvent::PostTool, Some(name), &payload, cancel).await {
            return (format!("{output}\n\n[post_tool hook: {why}]"), is_error);
        }
    }
    (output, is_error)
}

/// Decides whether a tool call may run.
struct Gate<'a> {
    /// In plan mode, MCP tools that don't say they only read.
    plan_denied: &'a [String],
    agent: &'a Agent,
    config: &'a AppConfig,
    pending: &'a Pending,
    mode: Mode,
    cancel: &'a CancellationToken,
}

impl Gate<'_> {
    /// Ok to run it; Err with the tool result to send back instead.
    #[allow(clippy::too_many_arguments)]
    async fn authorize(
        &self,
        session: &mut SessionLog,
        ctx: &mut ContextState,
        events: &Events,
        approvals: &mut Vec<crate::permissions::Rule>,
        id: &str,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<(), String> {
        if self.cancel.is_cancelled() {
            return Err("[Tool error: cancelled by user]".to_string());
        }
        let policy = Policy::new(self.config.permissions.clone())
            .with_approvals(approvals.clone())
            .with_mode(self.mode)
            .deny_tools(self.plan_denied);
        let decision = policy.decide(name, input);
        // What the user is asked about: every part that needs it.
        let asked = decision.asks.join("  ·  ");
        let log = |session: &mut SessionLog, ctx: &mut ContextState, allowed: bool, by: &str| {
            record(self.agent, session, ctx, events, EntryKind::Approval {
                tool_use_id: id.to_string(),
                decision: if allowed { "allow" } else { "deny" }.to_string(),
                by: by.to_string(),
                subject: if decision.action == Action::Ask { asked.clone() } else { decision.subject.clone() },
            });
            let _ = events.send(AgentEvent::PermissionDecided { id: id.to_string(), allowed, by: by.to_string() });
        };
        match decision.action {
            Action::Allow => Ok(()),
            Action::Deny => {
                log(session, ctx, false, "rule");
                if self.mode == Mode::Plan && crate::permissions::EDITING_TOOLS.contains(&name) {
                    Err("[Plan mode: file edits are turned off. Describe the change instead; the user can \
                         switch to build mode to make it.]"
                        .to_string())
                } else {
                    Err(format!(
                        "[Permission denied: a rule doesn't allow `{}`. Try another way, or ask the user.]",
                        crate::session::one_line(&decision.subject, 200)
                    ))
                }
            }
            Action::Ask => {
                let mut note = None;
                if let Some(reviewer) = &self.config.reviewer {
                    let task = crate::session::user_prompts(session.entries()).last().map(|p| p.text.clone()).unwrap_or_default();
                    match crate::control::review(
                        self.agent.model.as_ref(),
                        self.config,
                        reviewer,
                        &task,
                        name,
                        input,
                        self.cancel.clone(),
                    )
                    .await
                    {
                        Review::Allow(why) => {
                            log(session, ctx, true, "reviewer");
                            let _ = events.send(AgentEvent::Notice {
                                message: format!(
                                    "[Syl] Reviewer allowed `{}`: {}",
                                    crate::session::one_line(&asked, 80),
                                    crate::session::one_line(&why, 200)
                                ),
                            });
                            return Ok(());
                        }
                        Review::Block(why) => note = Some(format!("The reviewer would block it: {why}")),
                        Review::Unsure(why) => note = Some(format!("No review: {why}")),
                    }
                }
                // A cancel during the review: don't ask about a turn that's over.
                if self.cancel.is_cancelled() {
                    return Err("[Tool error: cancelled by user]".to_string());
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                self.pending.lock().unwrap().insert(id.to_string(), tx);
                let _ = events.send(AgentEvent::PermissionRequested {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: input.clone(),
                    subject: asked.clone(),
                    note,
                });
                let answer = tokio::select! {
                    a = rx => a.ok(),
                    _ = self.cancel.cancelled() => None,
                };
                self.pending.lock().unwrap().remove(id);
                match answer {
                    Some(PermissionAnswer::Once) => {
                        log(session, ctx, true, "user");
                        Ok(())
                    }
                    Some(PermissionAnswer::Always) => {
                        log(session, ctx, true, "user_always");
                        if let Some(path) = &self.agent.paths.approvals {
                            for subject in &decision.asks {
                                match crate::permissions::save_approval(path, name, subject) {
                                    Ok(rule) => approvals.push(rule),
                                    Err(e) => {
                                        let _ = events.send(AgentEvent::Notice {
                                            message: format!("[Syl] Allowed once; couldn't save the rule: {e}"),
                                        });
                                        break;
                                    }
                                }
                            }
                        }
                        Ok(())
                    }
                    Some(PermissionAnswer::Reject) => {
                        log(session, ctx, false, "user");
                        Err("[The user declined this tool call. Don't retry it; ask or take another approach.]".to_string())
                    }
                    None => Err("[Tool error: cancelled by user]".to_string()),
                }
            }
        }
    }
}
