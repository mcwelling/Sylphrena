//! The agent loop, driven by commands and observed through events.
//!
//! `spawn` starts the loop on a task. A client sends `AgentCommand`s and
//! reads `AgentEvent`s; it never needs a terminal. Every `Prompt` produces
//! `TurnStarted`, then any number of other events, then exactly one
//! `TurnEnded`.
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use syl_container::Container;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::client::{BlockDeltaEvent, CancelledError, SystemBlock};
use crate::api::compaction;
use crate::api::types::{ContentBlock, Message, MessageContent, StopReason, Usage};
use crate::config::AppConfig;
use crate::model::{ModelAdapter, ModelRequest};
use crate::shared::SharedState;
use crate::system_prompt::build_system_blocks;
use crate::tools::{self, ToolContext};
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
    /// Status text: retry cool-downs, container recovery.
    Notice { message: String },
    Error { message: String },
    TurnEnded { reason: TurnEnd },
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
    /// Stop the loop: cancel any turn in progress (it still ends with
    /// `TurnEnded` and is saved), drop queued prompts, and return. The loop
    /// also stops once every command sender is dropped, but a client that
    /// hands clones to signal handlers should send this instead.
    Shutdown,
}

/// Where a session keeps its files.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub project_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub workflow_dir: PathBuf,
    /// Saved after every turn when set.
    pub session_path: Option<PathBuf>,
}

impl SessionPaths {
    /// The standard layout under `<project>/.syl`.
    pub fn for_project(project_dir: impl Into<PathBuf>) -> Self {
        let project_dir = project_dir.into();
        let syl_dir = project_dir.join(".syl");
        SessionPaths {
            memory_dir: syl_dir.join("memory"),
            workflow_dir: syl_dir.join("workflows"),
            session_path: Some(syl_dir.join("session.json")),
            project_dir,
        }
    }
}

/// Everything the loop runs with.
pub struct Agent {
    pub config: AppConfig,
    pub state: Arc<SharedState>,
    pub model: Arc<dyn ModelAdapter>,
    pub container: Arc<Container>,
    pub paths: SessionPaths,
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

async fn run(
    agent: Agent,
    mut commands: mpsc::UnboundedReceiver<AgentCommand>,
    events: mpsc::UnboundedSender<AgentEvent>,
) {
    let mut queued: VecDeque<String> = VecDeque::new();
    loop {
        let prompt = match queued.pop_front() {
            Some(p) => p,
            None => match commands.recv().await {
                None | Some(AgentCommand::Shutdown) => break,
                Some(AgentCommand::Prompt(p)) => p,
                Some(AgentCommand::SetModel(m)) => {
                    *agent.state.current_model.lock().unwrap() = m;
                    continue;
                }
                Some(AgentCommand::Cancel | AgentCommand::ApprovePermission { .. }) => continue,
            },
        };

        agent.state.messages.lock().unwrap().push(Message::user_text(&prompt));
        let _ = events.send(AgentEvent::TurnStarted);

        let cancel = CancellationToken::new();
        let turn = run_turn(&agent, &events, cancel.clone());
        tokio::pin!(turn);
        let mut shutting_down = false;
        let reason = loop {
            tokio::select! {
                reason = &mut turn => break reason,
                cmd = commands.recv(), if !shutting_down => match cmd {
                    Some(AgentCommand::Shutdown) | None => {
                        shutting_down = true;
                        cancel.cancel();
                    }
                    Some(AgentCommand::Cancel) => cancel.cancel(),
                    Some(AgentCommand::Prompt(p)) => queued.push_back(p),
                    // Takes effect next turn; this one already chose its model.
                    Some(AgentCommand::SetModel(m)) => *agent.state.current_model.lock().unwrap() = m,
                    Some(AgentCommand::ApprovePermission { .. }) => {}
                },
            }
        };

        if let Some(path) = &agent.paths.session_path {
            let msgs = agent.state.messages.lock().unwrap().clone();
            if let Err(e) = crate::session::save_session(path, &msgs) {
                tracing::warn!("session save failed: {e}");
            }
        }
        let _ = events.send(AgentEvent::TurnEnded { reason });
        if shutting_down {
            break;
        }
    }
}

/// The system prompt for this conversation: built while history has no
/// assistant turn yet, then frozen (see `SharedState::system_prompt`).
fn system_prompt(agent: &Agent) -> Vec<SystemBlock> {
    let fresh_conversation = !agent
        .state
        .messages
        .lock()
        .unwrap()
        .iter()
        .any(|m| m.role == "assistant");
    let mut cached = agent.state.system_prompt.lock().unwrap();
    if fresh_conversation || cached.is_none() {
        let workspace = gather_workspace_context(&agent.paths.project_dir);
        let blocks = build_system_blocks(&workspace, &agent.paths.memory_dir)
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
        *cached = Some(blocks);
    }
    cached.clone().unwrap_or_default()
}

/// One user turn: model calls and tool calls until the model stops.
async fn run_turn(
    agent: &Agent,
    events: &mpsc::UnboundedSender<AgentEvent>,
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

    loop {
        emit(AgentEvent::RequestStarted);
        let system = system_prompt(agent);

        // Compact before every model call, not just between user messages:
        // a long tool chain inside one turn can otherwise grow without bound.
        {
            let msgs = agent.state.messages.lock().unwrap().clone();
            let before = msgs.len();
            if before > compaction::MAX_HISTORY {
                let compacted =
                    compaction::compact_history(msgs, agent.model.as_ref(), &config, cancel.clone())
                        .await;
                let after = compacted.len();
                *agent.state.messages.lock().unwrap() = compacted;
                if after != before {
                    emit(AgentEvent::Compacted { before, after });
                }
            }
        }
        if cancel.is_cancelled() {
            return TurnEnd::Cancelled;
        }

        let messages = agent.state.messages.lock().unwrap().clone();
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
            Err(e) => {
                emit(AgentEvent::Error { message: e.to_string() });
                return TurnEnd::Failed;
            }
        };

        let session = {
            let mut tok = agent.state.session_tokens.lock().unwrap();
            tok.input_tokens += turn.usage.input_tokens;
            tok.output_tokens += turn.usage.output_tokens;
            tok.cache_read_tokens += turn.usage.cache_read_tokens;
            tok.cache_write_tokens += turn.usage.cache_write_tokens;
            tok.clone()
        };
        emit(AgentEvent::Usage { request: turn.usage.clone(), session });

        // A tool_use the model did not finish (stop_reason max_tokens etc.)
        // gets no tool_result, and a tool_use without one makes every later
        // request fail with a 400, so only a tool_use turn keeps them.
        agent.state.messages.lock().unwrap().push(Message::assistant_blocks(
            compaction::drop_unanswered_tool_uses(&turn.content, &turn.stop_reason),
        ));

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

        let ctx = ToolContext {
            config: &config,
            model: agent.model.as_ref(),
            container: &agent.container,
            memory_dir: &agent.paths.memory_dir,
            workflow_dir: &agent.paths.workflow_dir,
        };
        let mut results = Vec::with_capacity(calls.len());
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
                let (output, is_error) = tools::execute(&name, &input, &ctx, cancel.clone()).await;
                emit(AgentEvent::ToolCallFinished {
                    id: id.clone(),
                    name,
                    output: output.clone(),
                    is_error,
                });
                (output, is_error)
            };
            results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: output,
                is_error: is_error.then_some(true),
            });
        }
        agent.state.messages.lock().unwrap().push(Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(results),
        });

        if cancel.is_cancelled() {
            return TurnEnd::Cancelled;
        }
    }
}
