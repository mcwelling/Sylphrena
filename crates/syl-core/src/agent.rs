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
use crate::api::types::{ContentBlock, Message, StopReason, Usage};
use crate::config::AppConfig;
use crate::model::{ModelAdapter, ModelRequest};
use crate::session::{EntryKind, PromptBlock, SessionInfo, SessionLog};
use crate::shared::SharedState;
use crate::system_prompt::build_system_blocks;
use crate::tools::{self, ToolContext};
use crate::util::SpillDir;
use crate::workspace::gather_workspace_context;

/// What happened in the agent, in order.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// A prompt was accepted and its turn began.
    TurnStarted,
    /// A model request is about to be sent (once per step of a tool chain).
    RequestStarted,
    TextDelta { text: String },
    ThinkingDelta { text: String },
    ToolCallStarted { id: String, name: String, input: serde_json::Value },
    ToolCallFinished { id: String, name: String, output: String, is_error: bool },
    /// Reserved for the permission engine (Phase 3). The default policy
    /// allows everything, so nothing emits it yet.
    PermissionRequested { id: String, name: String, input: serde_json::Value },
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
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    /// The model stopped for this reason (never `ToolUse`).
    Completed(StopReason),
    Cancelled,
    /// An `Error` event with the cause came just before.
    Failed,
}

/// What a client can ask of the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentCommand {
    /// Send a user message. Prompts sent during a turn run after it, in order.
    Prompt(String),
    /// Stop the turn in progress. Ignored when idle.
    Cancel,
    /// Answer a `PermissionRequested` event. Ignored until Phase 3.
    ApprovePermission { id: String, approved: bool },
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
            AgentCommand::SetModel(m) => {
                *agent.state.current_model.lock().unwrap() = m;
                continue;
            }
            AgentCommand::NewSession => {
                session = SessionLog::create(dir, cwd);
                switch_to(&agent, &session, &mut ctx, &events);
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
            AgentCommand::Prompt(p) => p,
        };

        emit(AgentEvent::TurnStarted);
        record(&agent, &mut session, &mut ctx, &events, EntryKind::Message {
            message: Message::user_text(&prompt),
        });

        let cancel = CancellationToken::new();
        let mut shutting_down = false;
        let reason = {
            let turn = run_turn(&agent, &mut session, &mut ctx, &events, cancel.clone());
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    reason = &mut turn => break reason,
                    cmd = commands.recv(), if !shutting_down => match cmd {
                        Some(AgentCommand::Shutdown) | None => {
                            shutting_down = true;
                            cancel.cancel();
                        }
                        Some(AgentCommand::Cancel) => cancel.cancel(),
                        // Takes effect next turn; this one already chose its
                        // model. Behind queued prompts it waits its turn.
                        Some(AgentCommand::SetModel(m)) if deferred.is_empty() => {
                            *agent.state.current_model.lock().unwrap() = m
                        }
                        Some(AgentCommand::ApprovePermission { .. }) => {}
                        // Prompts and session commands wait for the turn.
                        Some(other) => deferred.push_back(other),
                    },
                }
            }
        };
        emit(AgentEvent::TurnEnded { reason });
        if shutting_down {
            break;
        }
    }
}

/// Make `session` the one the shared state shows.
fn adopt(agent: &Agent, session: &SessionLog) {
    *agent.state.messages.lock().unwrap() = session.messages().to_vec();
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
        EntryKind::Message { .. } | EntryKind::ToolResult { .. } => Some(false),
        EntryKind::Compaction { .. } | EntryKind::Resumed => Some(true),
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
    let workspace = gather_workspace_context(&agent.paths.project_dir);
    let blocks: Vec<SystemBlock> = build_system_blocks(&workspace, &agent.paths.memory_dir)
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
fn context_estimate(ctx: &ContextState, messages: &[Message], system: &[SystemBlock]) -> u64 {
    match ctx.measured {
        Some((n, tokens)) if n <= messages.len() => {
            tokens + messages[n..].iter().map(compaction::estimate_tokens).sum::<u64>()
        }
        _ => {
            messages.iter().map(compaction::estimate_tokens).sum::<u64>()
                + system.iter().map(|b| compaction::estimate_text_tokens(&b.text)).sum::<u64>()
                + compaction::estimate_text_tokens(&syl_tools().to_string())
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
async fn run_turn(
    agent: &Agent,
    session: &mut SessionLog,
    ctx: &mut ContextState,
    events: &Events,
    cancel: CancellationToken,
) -> TurnEnd {
    let emit = |e: AgentEvent| {
        let _ = events.send(e);
    };

    // The model is fixed for the whole turn.
    let config = {
        let mut c = agent.config.clone();
        let model = agent.state.current_model.lock().unwrap().clone();
        if !model.is_empty() {
            c.model = model;
        }
        c
    };
    if session.history().model.as_deref() != Some(config.model.as_str()) {
        record(agent, session, ctx, events, EntryKind::ModelChange { model: config.model.clone() });
    }
    let spill = agent.paths.spill.as_ref().map(|s| SpillDir {
        host: s.host.join(session.id()),
        container: format!("{}/{}", s.container.trim_end_matches('/'), session.id()),
    });

    let mut overflow_retries = 0;
    // The error of a request rejected as too long, while it is retried.
    let mut overflow: Option<String> = None;
    loop {
        emit(AgentEvent::RequestStarted);
        let system = system_prompt(agent, session, ctx, events);

        // Compact before every model call, not just between user messages:
        // a long tool chain inside one turn can otherwise grow without bound.
        // Mid-chain (the last message holds tool results) compaction strips
        // thinking from the turn in progress, so it waits until the context
        // is twice as close to full.
        let window = context_window(agent, &config, ctx);
        let estimate = context_estimate(ctx, session.messages(), &system)
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

        if let Err(e) = agent.container.ensure(&on_notice) {
            emit(AgentEvent::Notice { message: format!("[Syl] Container recovery failed: {e}") });
        }

        for (id, name, input) in calls {
            // Every tool_use must get a tool_result, even after a cancel.
            let (output, is_error) = if cancel.is_cancelled() {
                ("[Tool error: cancelled by user]".to_string(), true)
            } else {
                emit(AgentEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                });
                let tool_ctx = ToolContext {
                    config: &config,
                    model: agent.model.as_ref(),
                    container: &agent.container,
                    memory_dir: &agent.paths.memory_dir,
                    workflow_dir: &agent.paths.workflow_dir,
                    spill: spill.as_ref(),
                };
                let (output, is_error) = tools::execute(&name, &id, &input, &tool_ctx, cancel.clone()).await;
                emit(AgentEvent::ToolCallFinished {
                    id: id.clone(),
                    name,
                    output: output.clone(),
                    is_error,
                });
                (output, is_error)
            };
            // Logged as soon as it exists, so a crash keeps finished work.
            record(agent, session, ctx, events, EntryKind::ToolResult {
                tool_use_id: id,
                content: output,
                is_error,
            });
        }

        if cancel.is_cancelled() {
            return TurnEnd::Cancelled;
        }
    }
}
