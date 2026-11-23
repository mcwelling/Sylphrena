//! `run_bash` tool: runs a command inside the project container.
use std::time::Duration;

use syl_container::{Container, ExecError};
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::util::{process_tool_output_spilled, SpillDir};

/// Execute `run_bash`. Returns `(output, is_error)`.
///
/// Times out after `config.timeout_ms`; on cancellation the process is killed.
/// Output too long to show is saved under `spill` as `<call_id>.txt`.
#[tracing::instrument(skip_all, fields(cmd = input["command"].as_str().unwrap_or("")))]
pub async fn run_bash(
    input: &serde_json::Value,
    config: &AppConfig,
    container: &Container,
    spill: Option<&SpillDir>,
    call_id: &str,
    cancel: CancellationToken,
) -> (String, bool) {
    let Some(cmd) = input["command"].as_str() else {
        return ("[Tool error: run_bash requires a 'command' field]".to_string(), true);
    };
    match container
        .exec(cmd, Duration::from_millis(config.timeout_ms), &cancel)
        .await
    {
        Ok(out) if out.success() => (process_tool_output_spilled(&out.stdout, spill, call_id), false),
        Ok(out) => (process_tool_output_spilled(&out.error_text(), spill, call_id), true),
        Err(ExecError::TimedOut(_)) => (
            format!("[Tool error: command timed out after {}s]", config.timeout_ms / 1000),
            true,
        ),
        Err(e) => (format!("[Tool error: {e}]"), true),
    }
}
