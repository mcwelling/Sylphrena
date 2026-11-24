//! Podman container lifecycle and command execution for one project.
//!
//! This crate knows nothing about agents, models or terminals: it starts,
//! resumes and removes a per-project container and runs commands in it, so
//! Syl and Bondsmith can share it. Progress messages go to a caller-supplied
//! callback instead of stdout.
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

/// Image committed after the first cold start so later starts skip setup.
pub const BAKED_IMAGE: &str = "syl-agent-ready:latest";
/// Image used when no baked image exists yet.
pub const BASE_IMAGE: &str = "mcr.microsoft.com/devcontainers/base:ubuntu";
/// Container name used by Syl versions before per-project names existed.
pub const LEGACY_CONTAINER_NAME: &str = "syl-agent";

/// Where the project is mounted inside the container.
pub const WORKDIR: &str = "/workspace";

/// The `timeout` argument that bounds a command inside the container: a
/// little past the host's own limit, so the host normally reports it first.
fn in_container_limit(timeout: Duration) -> String {
    format!("{}s", timeout.as_secs() + 2)
}

/// Container name for a project: `syl-agent-<hash>`, where the hash is the
/// first 10 hex characters of SHA-256 of the project path. Each project gets
/// its own container.
pub fn name_for(project_dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(project_dir.to_string_lossy().as_bytes());
    format!("syl-agent-{}", &hex::encode(digest)[..10])
}

/// Quote `s` for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Captured output of a finished command.
#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    /// Exit code, or None if the process was killed by a signal.
    pub code: Option<i32>,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// `Error (Exit N): <stdout>\n<stderr>`, skipping empty streams.
    pub fn error_text(&self) -> String {
        let parts: Vec<&str> = [self.stdout.trim(), self.stderr.trim()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect();
        format!("Error (Exit {}): {}", self.code.unwrap_or(-1), parts.join("\n"))
    }

    fn from_output(out: Output) -> Self {
        ExecOutput {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            code: out.status.code(),
        }
    }
}

/// Why an async exec did not produce output.
#[derive(Debug)]
pub enum ExecError {
    Spawn(std::io::Error),
    Wait(std::io::Error),
    TimedOut(Duration),
    Cancelled,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Spawn(e) => write!(f, "failed to spawn podman exec: {e}"),
            ExecError::Wait(e) => write!(f, "process wait failed: {e}"),
            ExecError::TimedOut(d) => write!(f, "command timed out after {}s", d.as_secs()),
            ExecError::Cancelled => write!(f, "cancelled by user"),
        }
    }
}

impl std::error::Error for ExecError {}

/// CPU and memory snapshot from `podman stats`.
#[derive(Debug, Clone, Default)]
pub struct ContainerStats {
    pub cpu_percent: f64,
    pub mem_usage_mb: f64,
    pub mem_limit_mb: f64,
}

/// One project's container.
#[derive(Debug, Clone)]
pub struct Container {
    name: String,
    project_dir: PathBuf,
    state_dir: PathBuf,
    podman: PathBuf,
    /// Where the project is mounted inside the container.
    workdir: String,
    options: ContainerOptions,
}

/// Resource limits and network for the container (`[container]` in
/// config.toml). A container started with other options is recreated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerOptions {
    /// `--network`: `Some("none")` for no network at all.
    pub network: Option<String>,
    /// `--memory`.
    pub memory: String,
    /// `--cpus`, when limited.
    pub cpus: Option<String>,
}

impl Default for ContainerOptions {
    fn default() -> Self {
        ContainerOptions { network: None, memory: "2g".to_string(), cpus: None }
    }
}

impl ContainerOptions {
    /// Recorded as a label on the container, to tell when it must be
    /// recreated.
    fn label(&self) -> String {
        format!(
            "memory={};cpus={};network={}",
            self.memory,
            self.cpus.as_deref().unwrap_or(""),
            self.network.as_deref().unwrap_or("")
        )
    }
}

const OPTIONS_LABEL: &str = "syl.options";

/// What `exec_argv` returns: stdout as bytes (file contents need not be
/// UTF-8), stderr as text.
#[derive(Debug, Clone)]
pub struct RawOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub code: Option<i32>,
}

impl RawOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

impl Container {
    /// The container for `project_dir`. The path is canonicalised so the
    /// name is stable however the directory was reached. Syl's state
    /// directory defaults to `<project>/.syl`, mounted read-only.
    pub fn for_project(project_dir: &Path) -> Self {
        let dir = project_dir
            .canonicalize()
            .unwrap_or_else(|_| project_dir.to_path_buf());
        Container {
            name: name_for(&dir),
            state_dir: dir.join(".syl"),
            project_dir: dir,
            podman: PathBuf::from("podman"),
            workdir: WORKDIR.to_string(),
            options: ContainerOptions::default(),
        }
    }

    /// Start the container with these limits and network.
    pub fn with_options(mut self, options: ContainerOptions) -> Self {
        self.options = options;
        self
    }

    pub fn options(&self) -> &ContainerOptions {
        &self.options
    }

    /// Whether the running container was started with other options. One
    /// from before options were recorded counts as having the defaults.
    fn options_changed(&self) -> bool {
        let Ok(out) = self
            .podman()
            .args(["inspect", "--format", &format!("{{{{index .Config.Labels \"{OPTIONS_LABEL}\"}}}}"), &self.name])
            .output()
        else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        let label = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let label = if label.is_empty() || label == "<no value>" {
            ContainerOptions::default().label()
        } else {
            label
        };
        label != self.options.label()
    }

    /// Treat `dir` as the in-container project path. Tests pair this with a
    /// stand-in podman that runs commands on the host.
    pub fn with_workdir(mut self, dir: impl Into<String>) -> Self {
        self.workdir = dir.into();
        self
    }

    /// The project's path inside the container (`/workspace`).
    pub fn workdir(&self) -> &str {
        &self.workdir
    }

    /// `path` inside the container: relative paths are under `workdir`.
    pub fn resolve(&self, path: &str) -> String {
        let path = path.trim();
        if path.starts_with('/') {
            path.to_string()
        } else {
            let rel = path.trim_start_matches("./");
            if rel.is_empty() || rel == "." {
                self.workdir.clone()
            } else {
                format!("{}/{}", self.workdir.trim_end_matches('/'), rel)
            }
        }
    }

    /// Use a different `podman` binary (tests use a stand-in script).
    pub fn with_podman(mut self, bin: impl Into<PathBuf>) -> Self {
        self.podman = bin.into();
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn podman(&self) -> Command {
        Command::new(&self.podman)
    }

    // -----------------------------------------------------------------------
    // State queries
    // -----------------------------------------------------------------------

    fn marker_path(&self) -> PathBuf {
        self.state_dir.join("container-ready")
    }

    fn read_marker(&self) -> Option<String> {
        std::fs::read_to_string(self.marker_path())
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn write_marker(&self) -> anyhow::Result<()> {
        let id = self.inspect_id()?;
        std::fs::write(self.marker_path(), id)?;
        Ok(())
    }

    fn inspect_id(&self) -> anyhow::Result<String> {
        let out = self
            .podman()
            .args(["inspect", "--format", "{{.Id}}", &self.name])
            .output()?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            anyhow::bail!("podman inspect failed: {}", String::from_utf8_lossy(&out.stderr))
        }
    }

    /// Whether the container is running.
    pub fn alive(&self) -> bool {
        match self
            .podman()
            .args(["inspect", "-f", "{{.State.Running}}", &self.name])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().contains("true"),
            Err(_) => false,
        }
    }

    fn baked_image_exists(&self) -> bool {
        matches!(
            self.podman().args(["image", "exists", BAKED_IMAGE]).output(),
            Ok(o) if o.status.success()
        )
    }

    fn workspace_mounted(&self) -> bool {
        matches!(
            self.podman()
                .args(["exec", &self.name, "mountpoint", "-q", "/workspace"])
                .output(),
            Ok(o) if o.status.success()
        )
    }

    fn run_podman(&self, args: &[&str]) -> anyhow::Result<()> {
        let status = self.podman().args(args).status()?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!(
                "podman {} failed (exit {})",
                args.first().copied().unwrap_or(""),
                status.code().unwrap_or(-1)
            )
        }
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Resume the container if the marker matches the live one, otherwise
    /// remove any stale one and cold-start. `fresh` also removes the baked
    /// image.
    pub fn start_or_resume(&self, fresh: bool, progress: &dyn Fn(&str)) -> anyhow::Result<()> {
        let saved_id = if fresh { None } else { self.read_marker() };
        let live_id = if saved_id.is_some() { self.inspect_id().ok() } else { None };

        let options_changed = saved_id.is_some() && saved_id == live_id && self.options_changed();
        if options_changed {
            progress("[Syl] Container options changed — recreating the container...");
        }
        if saved_id.is_some() && saved_id == live_id && !options_changed {
            progress("Resuming container...");
            let _ = self.run_podman(&["start", &self.name]);
            self.configure_git_and_ssh();
            if !self.workspace_mounted() {
                progress("[Syl] Workspace bind mount lost after warm start — rebuilding container...");
                self.remove(false);
                self.cold_start(progress)?;
            }
        } else {
            self.remove_legacy();
            self.remove(fresh);
            self.cold_start(progress)?;
        }

        self.write_marker()
    }

    /// Restart the container if it has died. Returns true when it had to be
    /// restored.
    pub fn ensure(&self, progress: &dyn Fn(&str)) -> anyhow::Result<bool> {
        if self.alive() {
            return Ok(false);
        }
        progress("[Syl] Container is gone — resummoning...");
        self.start_or_resume(false, progress)?;
        progress("[Syl] Container restored. Continuing.");
        Ok(true)
    }

    /// Remove the container; `hard` also removes the baked image.
    pub fn remove(&self, hard: bool) {
        let _ = self.podman().args(["rm", "-f", &self.name]).output();
        if hard {
            let _ = self.podman().args(["rmi", "-f", BAKED_IMAGE]).output();
        }
    }

    /// Remove a container left by an older Syl that used the single shared
    /// name `syl-agent`, but only when its /workspace is this project.
    fn remove_legacy(&self) {
        let Ok(out) = self
            .podman()
            .args([
                "inspect",
                "--format",
                "{{range .Mounts}}{{if eq .Destination \"/workspace\"}}{{.Source}}{{end}}{{end}}",
                LEGACY_CONTAINER_NAME,
            ])
            .output()
        else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let source = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !source.is_empty() && Path::new(&source) == self.project_dir {
            let _ = self.podman().args(["rm", "-f", LEGACY_CONTAINER_NAME]).output();
        }
    }

    fn cold_start(&self, progress: &dyn Fn(&str)) -> anyhow::Result<()> {
        let home = std::env::var("HOME").unwrap_or_default();
        let ssh_dir = format!("{home}/.ssh");
        let has_bake = self.baked_image_exists();
        if !has_bake && self.options.network.as_deref() == Some("none") {
            anyhow::bail!(
                "the first start installs Syl's tools from the network, but config.toml sets \
                 [container] network = \"none\". Start once without it, then turn it back on \
                 (the tools are kept in the {BAKED_IMAGE} image)."
            );
        }
        let base = if has_bake { BAKED_IMAGE } else { BASE_IMAGE };
        let project = self.project_dir.to_string_lossy().to_string();

        progress(&format!("Summoning a more powerful Syl in: {project}"));
        if has_bake {
            progress("[Syl Setup] Using cached image — skipping dependency install.");
        }

        // .syl/ is mounted read-only over the workspace copy so code running
        // in the container can't rewrite Syl's memory (injected into the
        // system prompt), workflows (run as commands), session or log.
        let _ = std::fs::create_dir_all(&self.state_dir);
        let ws_vol = format!("{project}:/workspace:rw");
        let state_vol = format!("{}:/workspace/.syl:ro", self.state_dir.to_string_lossy());
        let ssh_vol = format!("{ssh_dir}:/root/.ssh:ro");
        let term_program = format!(
            "TERM_PROGRAM={}",
            std::env::var("TERM_PROGRAM").unwrap_or_default()
        );

        let memory = format!("--memory={}", self.options.memory);
        let label = format!("{OPTIONS_LABEL}={}", self.options.label());
        let cpus = self.options.cpus.as_ref().map(|c| format!("--cpus={c}"));
        let network = self.options.network.as_ref().map(|n| format!("--network={n}"));
        let mut args: Vec<&str> = vec![
            "run", "-d", "--name", &self.name,
            "--label", &label,
            &memory,
            "-e", "TERM=xterm-256color",
            "-e", &term_program,
            "-v", &ws_vol,
            "-v", &state_vol,
        ];
        if Path::new(&ssh_dir).exists() {
            args.extend(["-v", &ssh_vol]);
        }
        args.extend(cpus.as_deref());
        args.extend(network.as_deref());
        args.extend(["--workdir", "/workspace", "-it", base, "/bin/bash"]);
        self.run_podman(&args)?;

        self.configure_git_and_ssh();

        if !has_bake {
            progress("[Syl Setup] Installing tools...");
            self.exec_raw(SETUP_TOOLS_CMD)
                .map_err(|e| anyhow::anyhow!("tool install failed: {e}"))?;
            progress("[Syl Setup] Baking image for fast restarts...");
            self.run_podman(&["commit", &self.name, BAKED_IMAGE])?;
            progress(&format!("[Syl Setup] Image cached as {BAKED_IMAGE}."));
        }
        Ok(())
    }

    fn configure_git_and_ssh(&self) {
        // Copy SSH keys (ssh refuses keys it doesn't own) and wrap ssh so it
        // uses the copied config and known_hosts.
        let home = std::env::var("HOME").unwrap_or_default();
        if Path::new(&format!("{home}/.ssh")).exists() {
            let _ = self.exec_raw(
                "bash -c '\
                    cp -rp /root/.ssh/. /tmp/.ssh/ 2>/dev/null || true \
                    && chown -R root:root /tmp/.ssh \
                    && chmod 700 /tmp/.ssh \
                    && find /tmp/.ssh -type f -exec chmod 600 {} \\; \
                    && find /tmp/.ssh -name \"*.pub\" -exec chmod 644 {} \\; \
                    && [ -f /tmp/.ssh/config ] || touch /tmp/.ssh/config \
                    && ssh-keyscan -H github.com >> /tmp/.ssh/known_hosts 2>/dev/null \
                    && printf \"#!/bin/sh\\nexec /usr/bin/ssh -F /tmp/.ssh/config -o UserKnownHostsFile=/tmp/.ssh/known_hosts \\\"\\$@\\\"\\n\" > /usr/local/bin/ssh \
                    && chmod +x /usr/local/bin/ssh\
                '",
            );
        }

        // Propagate the host's git identity.
        for key in ["user.name", "user.email"] {
            let value = Command::new("git")
                .args(["config", "--global", key])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if !value.is_empty() {
                let _ = self.exec_raw(&format!("git config --global {key} {}", shell_quote(&value)));
            }
        }
    }

    // -----------------------------------------------------------------------
    // Command execution
    // -----------------------------------------------------------------------

    /// Run `cmd` with `/bin/bash -c` in the container, blocking.
    pub fn exec_sync(&self, cmd: &str) -> std::io::Result<ExecOutput> {
        self.podman()
            .args(["exec", &self.name, "/bin/bash", "-c", cmd])
            .output()
            .map(ExecOutput::from_output)
    }

    /// Like `exec_sync`, but a non-zero exit is an error carrying
    /// `Error (Exit N): ...`. Used by setup steps and workflows.
    pub fn exec_raw(&self, cmd: &str) -> anyhow::Result<String> {
        let out = self.exec_sync(cmd)?;
        if out.success() {
            Ok(out.stdout)
        } else {
            anyhow::bail!("{}", out.error_text())
        }
    }

    /// Run a program (not a shell string) in the container with extra
    /// environment variables, returning raw output bytes.
    pub fn exec_program(&self, env: &[(&str, &str)], argv: &[&str]) -> std::io::Result<Output> {
        let mut cmd = self.podman();
        cmd.arg("exec");
        for (k, v) in env {
            cmd.arg("--env").arg(format!("{k}={v}"));
        }
        cmd.arg(&self.name).args(argv).output()
    }

    /// Run `cmd` with `/bin/bash -c` in the container. stdout and stderr are
    /// drained concurrently (reading them one after the other deadlocks once
    /// the unread pipe fills its ~64 KB buffer). The process is killed on
    /// timeout or cancellation.
    pub async fn exec(
        &self,
        cmd: &str,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<ExecOutput, ExecError> {
        let mut child = tokio::process::Command::new(&self.podman)
            .args(["exec", "--workdir", &self.workdir, &self.name])
            // Killing the host-side podman client doesn't stop the process
            // in the container, so it is bounded in there too.
            .args(["timeout", "-s", "KILL", &in_container_limit(timeout)])
            .args(["/bin/bash", "-c", cmd])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(ExecError::Spawn)?;

        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");

        async fn read_all(pipe: &mut (impl AsyncReadExt + Unpin)) -> String {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).into_owned()
        }

        tokio::select! {
            result = tokio::time::timeout(timeout, async {
                let (stdout, stderr) =
                    tokio::join!(read_all(&mut stdout_pipe), read_all(&mut stderr_pipe));
                let status = child.wait().await;
                (stdout, stderr, status)
            }) => match result {
                Ok((stdout, stderr, Ok(status))) => Ok(ExecOutput { stdout, stderr, code: status.code() }),
                Ok((_, _, Err(e))) => Err(ExecError::Wait(e)),
                Err(_) => {
                    let _ = child.kill().await;
                    Err(ExecError::TimedOut(timeout))
                }
            },
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                Err(ExecError::Cancelled)
            }
        }
    }

    /// Run `argv` (no shell) in the container, in the project directory,
    /// feeding it `stdin` when given. Output is drained concurrently; the
    /// process is killed on timeout or cancellation.
    pub async fn exec_argv(
        &self,
        argv: &[&str],
        stdin: Option<&[u8]>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<RawOutput, ExecError> {
        use tokio::io::AsyncWriteExt;
        let mut cmd = tokio::process::Command::new(&self.podman);
        cmd.arg("exec");
        if stdin.is_some() {
            cmd.arg("-i");
        }
        let mut child = cmd
            .args(["--workdir", &self.workdir, &self.name])
            .args(["timeout", "-s", "KILL", &in_container_limit(timeout)])
            .args(argv)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(ExecError::Spawn)?;

        let input = stdin.map(|b| b.to_vec());
        let mut stdin_pipe = child.stdin.take();
        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");

        let work = async {
            let feed = async {
                if let (Some(pipe), Some(bytes)) = (stdin_pipe.as_mut(), input.as_deref()) {
                    let _ = pipe.write_all(bytes).await;
                }
                drop(stdin_pipe.take());
            };
            let read_out = async {
                let mut buf = Vec::new();
                let _ = stdout_pipe.read_to_end(&mut buf).await;
                buf
            };
            let read_err = async {
                let mut buf = Vec::new();
                let _ = stderr_pipe.read_to_end(&mut buf).await;
                String::from_utf8_lossy(&buf).into_owned()
            };
            let ((), stdout, stderr) = tokio::join!(feed, read_out, read_err);
            (stdout, stderr, child.wait().await)
        };
        tokio::select! {
            result = tokio::time::timeout(timeout, work) => match result {
                Ok((stdout, stderr, Ok(status))) => Ok(RawOutput { stdout, stderr, code: status.code() }),
                Ok((_, _, Err(e))) => Err(ExecError::Wait(e)),
                Err(_) => Err(ExecError::TimedOut(timeout)),
            },
            _ = cancel.cancelled() => Err(ExecError::Cancelled),
        }
    }

    // -----------------------------------------------------------------------
    // Stats
    // -----------------------------------------------------------------------

    /// CPU and memory use, or None if the container is stopped or podman fails.
    pub fn stats(&self) -> Option<ContainerStats> {
        let out = self
            .podman()
            .args(["stats", "--no-stream", "--format", "json", &self.name])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let arr: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let item = arr.get(0)?;
        let cpu = item["CPUPerc"]
            .as_str()
            .and_then(|s| s.trim_end_matches('%').parse::<f64>().ok())
            .unwrap_or(0.0);
        // "MemUsage" looks like "123.4MB / 2GB".
        let mem = item["MemUsage"].as_str().unwrap_or("");
        let mut parts = mem.split('/');
        Some(ContainerStats {
            cpu_percent: cpu,
            mem_usage_mb: parse_mem_str(parts.next().unwrap_or("")),
            mem_limit_mb: parse_mem_str(parts.next().unwrap_or("")),
        })
    }
}

fn parse_mem_str(s: &str) -> f64 {
    let s = s.trim();
    let (n, scale) = if let Some(n) = s.strip_suffix("GB") {
        (n, 1024.0)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix("kB") {
        (n, 1.0 / 1024.0)
    } else {
        return 0.0;
    };
    n.trim().parse::<f64>().unwrap_or(0.0) * scale
}

const SETUP_TOOLS_CMD: &str = concat!(
    "apt-get update -qq && ",
    "apt-get install -y --no-install-recommends ",
    "python3 perl jq poppler-utils chafa python3-pil ripgrep ",
    "git curl wget unzip build-essential pkg-config libssl-dev && ",
    // timg is arch-specific — install best-effort so it never blocks the chain
    "apt-get install -y timg 2>/dev/null || echo '[Syl Setup] timg not available in repos, skipping' && ",
    // yq
    "wget -qO /usr/local/bin/yq https://github.com/mikefarah/yq/releases/latest/download/yq_linux_amd64 && ",
    "chmod +x /usr/local/bin/yq && ",
    // Rust toolchain via rustup
    "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal && ",
    // Symlink into /usr/local/bin so non-login bash shells find cargo/rustc
    "ln -sf /root/.cargo/bin/cargo /usr/local/bin/cargo && ",
    "ln -sf /root/.cargo/bin/rustc /usr/local/bin/rustc && ",
    "ln -sf /root/.cargo/bin/rustup /usr/local/bin/rustup"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_podman_memory_strings() {
        assert_eq!(parse_mem_str("2GB"), 2048.0);
        assert_eq!(parse_mem_str(" 123.5MB "), 123.5);
        assert_eq!(parse_mem_str("512kB"), 0.5);
        assert_eq!(parse_mem_str("?"), 0.0);
    }

    #[test]
    fn exec_error_text_skips_empty_streams() {
        let out = ExecOutput { stdout: "  ".into(), stderr: "boom\n".into(), code: Some(2) };
        assert_eq!(out.error_text(), "Error (Exit 2): boom");
    }
}
