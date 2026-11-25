//! Hooks: shell commands that run at points in a session.
//!
//! ```toml
//! [[hook]]
//! event = "pre_tool"           # session_start | pre_tool | post_tool | turn_end
//! tool = "run_bash"            # optional glob over the tool name (tool events)
//! command = "./scripts/check-command.sh"
//! timeout_secs = 30            # default 30
//! ```
//!
//! A hook gets a JSON object on stdin describing the event. A `pre_tool`
//! hook that exits non-zero blocks the call; what it printed (stderr, else
//! stdout) is the reason the model sees. Other events can't block; a failure
//! is reported as a notice.
//!
//! Hooks run **in the container**, like everything the agent runs. They come
//! from `.syl/config.toml`, which a cloned project can ship, so running them
//! on the host would mean a repository could run commands on your machine
//! the moment Syl starts.
use std::time::Duration;

use serde::Deserialize;
use syl_container::Container;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    PreTool,
    PostTool,
    TurnEnd,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub event: HookEvent,
    /// Shell command, run with `sh -c` in the project directory.
    pub command: String,
    /// For tool events: a glob over the tool name. Default: every tool.
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

/// What a hook run came to.
#[derive(Debug, Clone, PartialEq)]
pub enum HookOutcome {
    Ok,
    /// Exited non-zero (or couldn't run): the message to report.
    Failed(String),
}

/// The hooks for `event` (and, for tool events, this tool), in config order.
pub fn matching<'a>(hooks: &'a [Hook], event: HookEvent, tool: Option<&str>) -> Vec<&'a Hook> {
    hooks
        .iter()
        .filter(|h| h.event == event)
        .filter(|h| match (&h.tool, tool) {
            (Some(pattern), Some(t)) => crate::permissions::glob_match(pattern, t),
            (Some(_), None) => false,
            (None, _) => true,
        })
        .collect()
}

/// Run one hook in the container with `payload` on stdin.
pub async fn run(container: &Container, hook: &Hook, payload: &serde_json::Value, cancel: &CancellationToken) -> HookOutcome {
    let input = payload.to_string();
    let timeout = Duration::from_secs(hook.timeout_secs.max(1));
    match container.exec_argv(&["/bin/sh", "-c", &hook.command], Some(input.as_bytes()), timeout, cancel).await {
        Ok(out) if out.success() => HookOutcome::Ok,
        Ok(out) => {
            let said = if out.stderr.trim().is_empty() {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            } else {
                out.stderr.trim().to_string()
            };
            let said = crate::util::truncate_output(&said);
            HookOutcome::Failed(if said.is_empty() {
                format!("the hook `{}` exited with {}", hook.command, out.code.map_or("a signal".to_string(), |c| c.to_string()))
            } else {
                said
            })
        }
        Err(e) => HookOutcome::Failed(format!("the hook `{}` could not run: {e}", hook.command)),
    }
}

/// Run every matching hook in order; stop at the first failure.
pub async fn run_all(
    container: &Container,
    hooks: &[Hook],
    event: HookEvent,
    tool: Option<&str>,
    payload: &serde_json::Value,
    cancel: &CancellationToken,
) -> HookOutcome {
    for hook in matching(hooks, event, tool) {
        if let HookOutcome::Failed(why) = run(container, hook, payload, cancel).await {
            return HookOutcome::Failed(why);
        }
    }
    HookOutcome::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: HookEvent, tool: Option<&str>) -> Hook {
        Hook { event, command: "true".into(), tool: tool.map(Into::into), timeout_secs: 30 }
    }

    #[test]
    fn hooks_match_by_event_and_tool_glob() {
        let hooks = vec![
            hook(HookEvent::PreTool, Some("run_bash")),
            hook(HookEvent::PreTool, None),
            hook(HookEvent::PostTool, Some("*_file")),
            hook(HookEvent::TurnEnd, None),
        ];
        assert_eq!(matching(&hooks, HookEvent::PreTool, Some("run_bash")).len(), 2);
        assert_eq!(matching(&hooks, HookEvent::PreTool, Some("grep")).len(), 1);
        assert_eq!(matching(&hooks, HookEvent::PostTool, Some("edit_file")).len(), 1);
        assert_eq!(matching(&hooks, HookEvent::TurnEnd, None).len(), 1);
    }

    #[test]
    fn hook_config_parses() {
        let h: Hook = toml::from_str("event = \"pre_tool\"\ncommand = \"x\"\ntool = \"run_bash\"").unwrap();
        assert_eq!(h, Hook { event: HookEvent::PreTool, command: "x".into(), tool: Some("run_bash".into()), timeout_secs: 30 });
        assert!(toml::from_str::<Hook>("event = \"pre_tool\"\ncmd = \"x\"").is_err(), "unknown keys are errors");
    }
}
