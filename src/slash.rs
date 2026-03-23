use crate::api::types::Message;
use crate::config::AppConfig;
use crate::container::exec_command_raw;
use crate::session::save_session;
use crate::tools::memory::{list_memories, memory_read};
use crate::tools::workflow::{list_workflows, load_workflow};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum SlashAction {
    Continue,
    Exit,
}

#[derive(Debug, Clone)]
pub struct SlashResult {
    pub action:   SlashAction,
    pub messages: Vec<Message>,
    pub output:   Option<String>,
}

pub struct SlashContext<'a> {
    pub session_path:  &'a Path,
    pub workflow_dir:  &'a Path,
    pub memory_dir:    &'a Path,
    pub log_path:      &'a Path,
    pub model:         &'a str,
    pub backend:       &'a str,
    pub timeout_ms:    u64,
    pub rate_limit:    u64,
    pub input_tokens:  u64,
    pub output_tokens: u64,
    pub message_count: usize,
    pub config:        &'a AppConfig,
}

/// Handle a slash command. Returns None if the input is not a slash command.
pub async fn handle_slash(
    input:    &str,
    messages: Vec<Message>,
    ctx:      &SlashContext<'_>,
) -> Option<SlashResult> {
    let trimmed = input.trim();

    if trimmed == "/help" {
        let output = "\
Slash commands:
  /status           Session stats
  /save             Save session now
  /memory [key]     List memories or show one
  /workflow [name]  List workflows or run one
  /log              Last 20 lines of syl.log
  /clear            Clear conversation history
  /exit             Exit"
            .to_string();
        return Some(SlashResult { action: SlashAction::Continue, messages, output: Some(output) });
    }

    if trimmed == "/clear" {
        return Some(SlashResult { action: SlashAction::Continue, messages: vec![], output: None });
    }

    if trimmed == "/status" {
        let timeout_s = ctx.timeout_ms / 1000;
        let rate_display = if ctx.rate_limit == 0 {
            "unlimited".to_string()
        } else {
            format!("{} tpm", ctx.rate_limit)
        };
        let output = format!(
            "model:    {}  backend: {}\ntimeout:  {}s             rate:    {}\nmessages: {}               tokens:  in:{} out:{}\nsession:  {}",
            ctx.model,
            ctx.backend,
            timeout_s,
            rate_display,
            ctx.message_count,
            ctx.input_tokens,
            ctx.output_tokens,
            ctx.session_path.display(),
        );
        return Some(SlashResult { action: SlashAction::Continue, messages, output: Some(output) });
    }

    if trimmed == "/save" {
        let output = match save_session(ctx.session_path, &messages) {
            Ok(()) => "Session saved.".to_string(),
            Err(e) => format!("Save failed: {}", e),
        };
        return Some(SlashResult { action: SlashAction::Continue, messages, output: Some(output) });
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
        return Some(SlashResult { action: SlashAction::Continue, messages, output: Some(output) });
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
        return Some(SlashResult { action: SlashAction::Continue, messages, output: Some(output) });
    }

    if trimmed == "/workflow" || trimmed.starts_with("/workflow ") {
        let wf_name = trimmed.strip_prefix("/workflow").unwrap_or("").trim();
        let output = if wf_name.is_empty() {
            let entries = list_workflows(ctx.workflow_dir);
            if entries.is_empty() {
                "No workflows.".to_string()
            } else {
                entries.iter().map(|e| e.key.clone()).collect::<Vec<_>>().join("\n")
            }
        } else {
            match load_workflow(ctx.workflow_dir, wf_name) {
                None => format!("Workflow '{}' not found.", wf_name),
                Some(wf) => run_workflow(&wf, ctx.config).await,
            }
        };
        return Some(SlashResult { action: SlashAction::Continue, messages, output: Some(output) });
    }

    if trimmed == "/exit" || trimmed == "/quit" {
        return Some(SlashResult { action: SlashAction::Exit, messages, output: None });
    }

    if trimmed.starts_with('/') {
        // Unknown slash command
        return None;
    }

    None
}

// ---------------------------------------------------------------------------
// Workflow execution (M5)
// ---------------------------------------------------------------------------

async fn run_workflow(wf: &crate::tools::workflow::Workflow, config: &AppConfig) -> String {
    let total = wf.steps.len();
    let mut lines = vec![format!("[Workflow: {}] {} steps", wf.name, total)];

    for (i, step) in wf.steps.iter().enumerate() {
        let step_num = i + 1;
        lines.push(format!("[{step_num}/{total}] {}", step.label));
        lines.push(format!("$ {}", step.command));

        let cmd = step.command.clone();
        let cfg = config.clone();
        let result = tokio::task::block_in_place(|| exec_command_raw(&cmd, &cfg));

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
