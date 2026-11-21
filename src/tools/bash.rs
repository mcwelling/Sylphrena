/// `run_bash` tool — executes a command inside the Podman container.
/// Mirrors run-in-container from syl.bb, with tokio async + CancellationToken support.
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

use crate::config::{container_name, AppConfig};
use crate::util::process_tool_output;

/// In-flight child process handle. Stored so Ctrl+C can kill the subprocess.
pub type RunningProcess = Arc<Mutex<Option<u32>>>;

/// Execute `run_bash` tool. Returns `(output, is_error)`.
///
/// - Times out after `config.timeout_ms`.
/// - On cancellation, kills the running process and returns an error string.
#[tracing::instrument(skip_all, fields(cmd = input["command"].as_str().unwrap_or("")))]
pub async fn run_bash(
    input: &serde_json::Value,
    config: &AppConfig,
    _running_process: RunningProcess,
    cancel: CancellationToken,
) -> (String, bool) {
    let cmd = match input["command"].as_str() {
        Some(c) => c,
        None    => return ("[Tool error: run_bash requires a 'command' field]".to_string(), true),
    };

    let mut child = match Command::new("podman")
        .args(["exec", container_name(), "/bin/bash", "-c", cmd])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c)  => c,
        Err(e) => return (format!("[Tool error: failed to spawn podman exec: {}]", e), true),
    };

    let timeout_dur = Duration::from_millis(config.timeout_ms);

    // Take the output handles before entering select! so child doesn't need to move
    let mut stdout_pipe = child.stdout.take().unwrap();
    let mut stderr_pipe = child.stderr.take().unwrap();

    // Collect all output then wait for the process
    async fn read_all(pipe: &mut (impl AsyncReadExt + Unpin)) -> String {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).to_string()
    }

    tokio::select! {
        result = timeout(timeout_dur, async {
            // Drain both pipes concurrently: reading them one after the other
            // deadlocks once the unread pipe fills its ~64 KB OS buffer.
            let (stdout, stderr) =
                tokio::join!(read_all(&mut stdout_pipe), read_all(&mut stderr_pipe));
            let status = child.wait().await;
            (stdout, stderr, status)
        }) => {
            match result {
                Ok((stdout, stderr, Ok(status))) => {
                    if status.success() {
                        (process_tool_output(&stdout), false)
                    } else {
                        let code = status.code().unwrap_or(-1);
                        let combined = build_error_output(code, &stdout, &stderr);
                        (process_tool_output(&combined), true)
                    }
                }
                Ok((_, _, Err(e))) => (format!("[Tool error: process wait failed: {}]", e), true),
                Err(_) => {
                    let _ = child.kill().await;
                    (format!("[Tool error: command timed out after {}s]",
                             config.timeout_ms / 1000), true)
                }
            }
        }
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            ("[Tool error: cancelled by user]".to_string(), true)
        }
    }
}

fn build_error_output(code: i32, stdout: &str, stderr: &str) -> String {
    let parts: Vec<&str> = [stdout.trim(), stderr.trim()]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect();
    format!("Error (Exit {}): {}", code, parts.join("\n"))
}
