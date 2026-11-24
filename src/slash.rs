use std::path::Path;

use syl_container::Container;
use syl_core::agent::AgentCommand;
use syl_core::config::AppConfig;
use syl_core::session::{self, find_session, format_utc, list_sessions, load_entries, one_line, user_prompts};
use syl_core::tools::memory::{list_memories, memory_read};
use syl_core::tools::workflow::{list_workflows, load_workflow, Workflow};

#[derive(Debug, Clone, PartialEq)]
pub enum SlashAction {
    Continue,
    Exit,
    /// Send this to the agent and show its answer (`SessionChanged` or
    /// `Error`).
    Agent(AgentCommand),
}

#[derive(Debug, Clone)]
pub struct SlashResult {
    pub action: SlashAction,
    pub output: Option<String>,
}

impl SlashResult {
    fn say(output: impl Into<String>) -> Option<Self> {
        Some(SlashResult { action: SlashAction::Continue, output: Some(output.into()) })
    }

    fn agent(cmd: AgentCommand) -> Option<Self> {
        Some(SlashResult { action: SlashAction::Agent(cmd), output: None })
    }
}

pub struct SlashContext<'a> {
    /// Where session logs live.
    pub sessions_dir: &'a Path,
    /// The current session, once the agent has reported it.
    pub session_id: Option<&'a str>,
    pub workflow_dir: &'a Path,
    pub memory_dir: &'a Path,
    pub log_path: &'a Path,
    pub model: &'a str,
    pub timeout_ms: u64,
    pub rate_limit: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub message_count: usize,
    pub config: &'a AppConfig,
    pub container: &'a Container,
}

/// Handle a slash command. Returns None if the input is not a slash command.
pub async fn handle_slash(input: &str, ctx: &SlashContext<'_>) -> Option<SlashResult> {
    let trimmed = input.trim();

    if trimmed == "/help" {
        let output = "\
Slash commands:
  /model [name]     Interactive model picker, or switch directly by name
  /status           Session stats
  /tools            The last turn's tool calls in full (output, diffs)
  /sessions         List saved sessions
  /resume <n|id>    Switch to a saved session (number from /sessions, id or name)
  /history          Prompts in this session, numbered for /fork
  /fork [n]         Continue in a copy of this session (before prompt n)
  /name <name>      Name this session
  /clear            Start a new session
  /memory [key]     List memories or show one
  /workflow [name]  List workflows or run one
  /log              Last 20 lines of syl.log
  /exit             Exit";
        return SlashResult::say(output);
    }

    if trimmed == "/clear" || trimmed == "/new" {
        return SlashResult::agent(AgentCommand::NewSession);
    }

    if trimmed == "/save" {
        return SlashResult::say("Sessions are saved automatically as they happen.");
    }

    if trimmed == "/sessions" {
        let all = list_sessions(ctx.sessions_dir);
        if all.is_empty() {
            return SlashResult::say("No saved sessions.");
        }
        let lines: Vec<String> = all
            .iter()
            .take(30)
            .enumerate()
            .map(|(i, s)| {
                let current = if ctx.session_id == Some(s.id.as_str()) { "*" } else { " " };
                let label = match &s.name {
                    Some(n) => format!("[{n}] {}", s.title),
                    None => s.title.clone(),
                };
                format!(
                    "{current}{:>3}  {}  {}  {:>3} prompts  {}",
                    i + 1,
                    s.id,
                    format_utc(s.updated_ms),
                    s.prompt_count,
                    one_line(&label, 60),
                )
            })
            .collect();
        let more = all.len().saturating_sub(30);
        let mut out = lines.join("\n");
        if more > 0 {
            out.push_str(&format!("\n  … {more} older"));
        }
        return SlashResult::say(out);
    }

    if trimmed == "/resume" || trimmed.starts_with("/resume ") {
        let query = trimmed.strip_prefix("/resume").unwrap_or("").trim();
        if query.is_empty() {
            return SlashResult::say("Usage: /resume <n|id|name>  (see /sessions)");
        }
        return match find_session(ctx.sessions_dir, query) {
            Ok(id) => SlashResult::agent(AgentCommand::LoadSession(id)),
            Err(e) => SlashResult::say(format!("{e}")),
        };
    }

    if trimmed == "/history" {
        let prompts = current_prompts(ctx);
        if prompts.is_empty() {
            return SlashResult::say("No prompts in this session yet.");
        }
        let lines: Vec<String> = prompts
            .iter()
            .enumerate()
            .map(|(i, p)| format!("{:>3}  {}", i + 1, one_line(&p.text, 70)))
            .collect();
        return SlashResult::say(lines.join("\n"));
    }

    if trimmed == "/fork" || trimmed.starts_with("/fork ") {
        let arg = trimmed.strip_prefix("/fork").unwrap_or("").trim();
        if arg.is_empty() {
            return SlashResult::agent(AgentCommand::Fork { at: None });
        }
        let Ok(n) = arg.parse::<usize>() else {
            return SlashResult::say("Usage: /fork [n]  (n from /history)");
        };
        let prompts = current_prompts(ctx);
        return match n.checked_sub(1).and_then(|i| prompts.get(i)) {
            Some(p) => {
                let at = p.parent.clone();
                Some(SlashResult {
                    action: SlashAction::Agent(AgentCommand::Fork { at }),
                    output: Some(format!("Forking before prompt {n}: {}", one_line(&p.text, 70))),
                })
            }
            None => SlashResult::say(format!("No prompt {n}; this session has {}.", prompts.len())),
        };
    }

    if trimmed == "/name" || trimmed.starts_with("/name ") {
        let name = trimmed.strip_prefix("/name").unwrap_or("").trim();
        if name.is_empty() {
            return SlashResult::say("Usage: /name <name>");
        }
        return SlashResult::agent(AgentCommand::SetSessionName(name.to_string()));
    }

    if trimmed == "/status" {
        let timeout_s = ctx.timeout_ms / 1000;
        let rate_display = if ctx.rate_limit == 0 {
            "unlimited".to_string()
        } else {
            format!("{} tpm", ctx.rate_limit)
        };
        let output = format!(
            "model:    {}\ntimeout:  {}s             rate:    {}\nmessages: {}               tokens:  in:{} out:{}\nsession:  {}",
            ctx.model,
            timeout_s,
            rate_display,
            ctx.message_count,
            ctx.input_tokens,
            ctx.output_tokens,
            ctx.session_id.unwrap_or("(none)"),
        );
        return SlashResult::say(output);
    }

    if trimmed == "/memory" || trimmed.starts_with("/memory ") {
        let key = trimmed.strip_prefix("/memory").unwrap_or("").trim();
        let output = if key.is_empty() {
            let keys = list_memories(ctx.memory_dir);
            if keys.is_empty() {
                "No memories.".to_string()
            } else {
                keys.join("\n")
            }
        } else {
            let content = memory_read(ctx.memory_dir, key);
            if content.starts_with("No memory found") {
                format!("Memory '{}' not found.", key)
            } else {
                content
            }
        };
        return SlashResult::say(output);
    }

    if trimmed == "/log" {
        let output = match std::fs::read_to_string(ctx.log_path) {
            Ok(content) if !content.trim().is_empty() => {
                let lines: Vec<&str> = content.lines().collect();
                let start = lines.len().saturating_sub(20);
                lines[start..].join("\r\n")
            }
            _ => "No log entries yet.".to_string(),
        };
        return SlashResult::say(output);
    }

    if trimmed == "/workflow" || trimmed.starts_with("/workflow ") {
        let wf_name = trimmed.strip_prefix("/workflow").unwrap_or("").trim();
        let output = if wf_name.is_empty() {
            let entries = list_workflows(ctx.workflow_dir);
            if entries.is_empty() {
                "No workflows.".to_string()
            } else {
                entries
                    .iter()
                    .map(|e| e.key.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        } else {
            match load_workflow(ctx.workflow_dir, wf_name) {
                None => format!("Workflow '{}' not found.", wf_name),
                Some(wf) => run_workflow(&wf, ctx.container),
            }
        };
        return SlashResult::say(output);
    }

    if trimmed == "/exit" || trimmed == "/quit" {
        return Some(SlashResult { action: SlashAction::Exit, output: None });
    }

    if trimmed.starts_with('/') {
        // Unknown slash command
        return None;
    }

    None
}

/// The typed prompts of the current session, read from its log.
fn current_prompts(ctx: &SlashContext<'_>) -> Vec<session::Prompt> {
    ctx.session_id
        .and_then(|id| load_entries(ctx.sessions_dir, id).ok())
        .map(|entries| user_prompts(&entries))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Workflow execution (M5)
// ---------------------------------------------------------------------------

fn run_workflow(wf: &Workflow, container: &Container) -> String {
    let total = wf.steps.len();
    let mut lines = vec![format!("[Workflow: {}] {} steps", wf.name, total)];

    for (i, step) in wf.steps.iter().enumerate() {
        let step_num = i + 1;
        lines.push(format!("[{step_num}/{total}] {}", step.label));
        lines.push(format!("$ {}", step.command));

        let result = tokio::task::block_in_place(|| container.exec_raw(&step.command));

        match result {
            Ok(output) => {
                if !output.trim().is_empty() {
                    lines.push(output.trim().to_string());
                }
            }
            Err(e) => {
                lines.push(format!("Error: {e}"));
                lines.push(format!("[Workflow stopped at step {step_num}/{total}]"));
                return lines.join("\n");
            }
        }
    }

    lines.push(format!("[Workflow complete: {} steps]", total));
    lines.join("\n")
}
