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
    /// Held while starting or recovering the container, so two sessions
    /// sharing it don't both recreate it (one `rm -f`-ing the other's).
    lifecycle: std::sync::Arc<std::sync::Mutex<()>>,
    /// Worked out from `options` and podman on first use.
    resources: std::sync::Arc<std::sync::OnceLock<Resources>>,
    /// Out-of-memory kills counted so far (`u64::MAX` before the first
    /// count), to tell when a command lost processes to the memory limit.
    oom_seen: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Counter in each exec's environment, so the processes a command started
/// can be found and stopped.
static EXEC_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The variable that marks an exec's processes.
const EXEC_TAG: &str = "SYL_EXEC";

fn new_exec_tag() -> String {
    let n = EXEC_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{n}-{nanos}", std::process::id())
}

/// Kills every process whose environment has `SYL_EXEC=<tag>`: all a command
/// started, including ones that left its process group or session (a test
/// harness, `setsid`, a backgrounded server), which `timeout` and killing
/// the podman client both miss.
fn kill_tagged_script(tag: &str) -> String {
    format!(
        "for f in /proc/[0-9]*/environ; do \
           if tr '\\0' '\\n' < \"$f\" 2>/dev/null | grep -qxF '{EXEC_TAG}={tag}'; then \
             p=${{f#/proc/}}; kill -KILL \"${{p%/environ}}\" 2>/dev/null; \
           fi; \
         done; true"
    )
}

/// `oom_kill N` from cgroup v2's `memory.events` (or v1's `oom_control`).
fn parse_oom_kills(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|l| l.strip_prefix("oom_kill "))
        .and_then(|n| n.trim().parse().ok())
}

/// Resource limits and network for the container (`[container]` in
/// config.toml). A container started with other options is recreated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerOptions {
    /// `--network`: `Some("none")` for no network at all.
    pub network: Option<String>,
    /// `--memory`, as podman takes it (`"4g"`), or `"auto"`: a share of the
    /// memory podman has (the VM's, on macOS and Windows).
    pub memory: String,
    /// `--cpus`, when limited.
    pub cpus: Option<String>,
}

impl Default for ContainerOptions {
    fn default() -> Self {
        ContainerOptions { network: None, memory: "auto".to_string(), cpus: None }
    }
}

/// Share of podman's memory an `"auto"` container gets; the rest is left to
/// podman itself and other containers.
const AUTO_MEMORY_SHARE: f64 = 0.75;
/// Memory used when `"auto"` can't ask podman.
const FALLBACK_MEMORY_MB: u64 = 2048;
/// Memory a build job is given room for: about what compiling and linking
/// one crate or test binary takes at its peak.
const MB_PER_JOB: u64 = 1024;

/// What the container gets, worked out from the options and podman's own
/// memory and CPUs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    /// The `--memory` limit, in MB.
    pub memory_mb: u64,
    /// CPUs the container can use.
    pub cpus: u32,
    /// Parallel build jobs that fit: one per CPU, as many as the memory
    /// allows, at least one.
    pub jobs: u32,
}

impl Resources {
    /// From the options and podman's `(memory in bytes, CPUs)`, when known.
    pub fn plan(options: &ContainerOptions, podman: Option<(u64, u32)>) -> Resources {
        let podman_mb = podman.map(|(bytes, _)| bytes / (1024 * 1024)).filter(|&mb| mb > 0);
        let memory_mb = match parse_size_mb(&options.memory) {
            Some(mb) => mb,
            None => podman_mb
                .map(|mb| ((mb as f64 * AUTO_MEMORY_SHARE) as u64).max(512))
                .unwrap_or(FALLBACK_MEMORY_MB),
        };
        let podman_cpus = podman.map(|(_, c)| c).filter(|&c| c > 0).unwrap_or(2);
        let cpus = options
            .cpus
            .as_deref()
            .and_then(|c| c.trim().parse::<f64>().ok())
            .map(|c| (c.ceil() as u32).clamp(1, podman_cpus))
            .unwrap_or(podman_cpus);
        let jobs = ((memory_mb / MB_PER_JOB) as u32).clamp(1, cpus);
        Resources { memory_mb, cpus, jobs }
    }

    /// Environment variables that keep builds to `jobs` at once. Tools
    /// otherwise start one job per CPU they see, whatever the memory limit.
    pub fn job_env(&self) -> Vec<String> {
        let n = self.jobs;
        vec![
            format!("CARGO_BUILD_JOBS={n}"),
            format!("MAKEFLAGS=-j{n}"),
            format!("CMAKE_BUILD_PARALLEL_LEVEL={n}"),
        ]
    }

    /// `2.8 GB`, or `512 MB` under a GB.
    pub fn memory_text(&self) -> String {
        if self.memory_mb < 1024 {
            format!("{} MB", self.memory_mb)
        } else {
            format!("{:.1} GB", self.memory_mb as f64 / 1024.0)
        }
    }

    /// `1 build job`, `3 build jobs`.
    pub fn jobs_text(&self) -> String {
        if self.jobs == 1 { "1 build job".into() } else { format!("{} build jobs", self.jobs) }
    }
}

/// `"4g"`, `"512m"`, `"2048"` (bytes, as podman reads a bare number) to MB.
/// None for `"auto"` or anything else.
fn parse_size_mb(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_suffix('b').filter(|r| r.ends_with(['k', 'm', 'g'])).unwrap_or(&s);
    let (num, scale) = match s.chars().last()? {
        'g' => (&s[..s.len() - 1], 1024.0),
        'm' => (&s[..s.len() - 1], 1.0),
        'k' => (&s[..s.len() - 1], 1.0 / 1024.0),
        c if c.is_ascii_digit() => (s, 1.0 / (1024.0 * 1024.0)),
        _ => return None,
    };
    let n: f64 = num.trim().parse().ok().filter(|n: &f64| *n > 0.0)?;
    Some(((n * scale) as u64).max(1))
}

impl ContainerOptions {
    /// Recorded as a label on the container, to tell when it must be
    /// recreated. It holds the memory worked out, so resizing podman's VM
    /// resizes an `"auto"` container too.
    fn label(&self, res: &Resources) -> String {
        format!(
            "memory={}m;cpus={};network={}",
            res.memory_mb,
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
            lifecycle: Default::default(),
            resources: Default::default(),
            oom_seen: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX)),
        }
    }

    /// Start the container with these limits and network.
    pub fn with_options(mut self, options: ContainerOptions) -> Self {
        self.options = options;
        self.resources = Default::default();
        self
    }

    /// The memory, CPUs and build jobs the container gets. Asks podman for
    /// its memory and CPUs the first time.
    pub fn resources(&self) -> Resources {
        *self.resources.get_or_init(|| Resources::plan(&self.options, self.podman_capacity()))
    }

    /// podman's memory in bytes and CPUs: the VM's on macOS and Windows,
    /// the host's on Linux.
    fn podman_capacity(&self) -> Option<(u64, u32)> {
        let out = self
            .podman()
            .args(["info", "--format", "{{.Host.MemTotal}} {{.Host.CPUs}}"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut parts = text.split_whitespace();
        let mem = parts.next()?.parse().ok()?;
        let cpus = parts.next()?.parse().ok()?;
        Some((mem, cpus))
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
        // One from before options were recorded had a 2g limit.
        let label = if label.is_empty() || label == "<no value>" {
            "memory=2048m;cpus=;network=".to_string()
        } else {
            label
        };
        label != self.options.label(&self.resources())
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

    /// Run podman for its effect. Its stdout (container ids, names) is
    /// discarded: in `syl -p` and `syl acp` our stdout carries the answer or
    /// protocol messages, and a stray line would corrupt it. Its stderr goes
    /// into the error.
    fn run_podman(&self, args: &[&str]) -> anyhow::Result<()> {
        let out = self.podman().args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output()?;
        if out.status.success() {
            Ok(())
        } else {
            anyhow::bail!(
                "podman {} failed (exit {}): {}",
                args.first().copied().unwrap_or(""),
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
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
        let _held = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        self.start_or_resume_locked(fresh, progress)
    }

    fn start_or_resume_locked(&self, fresh: bool, progress: &dyn Fn(&str)) -> anyhow::Result<()> {
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

        // Kills from before this start aren't any command's.
        self.oom_seen.store(self.read_oom_kills().unwrap_or(u64::MAX), std::sync::atomic::Ordering::Relaxed);
        self.write_marker()
    }

    /// Restart the container if it has died. Returns true when it had to be
    /// restored.
    pub fn ensure(&self, progress: &dyn Fn(&str)) -> anyhow::Result<bool> {
        if self.alive() {
            return Ok(false);
        }
        let _held = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        // Another session may have brought it back while we waited.
        if self.alive() {
            return Ok(false);
        }
        progress("[Syl] Container is gone — resummoning...");
        self.start_or_resume_locked(false, progress)?;
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

        let res = self.resources();
        let memory = format!("--memory={}m", res.memory_mb);
        let label = format!("{OPTIONS_LABEL}={}", self.options.label(&res));
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
        let mut c = self.podman();
        c.arg("exec");
        for e in self.resources().job_env() {
            c.arg("--env").arg(e);
        }
        c.args([&self.name, "/bin/bash", "-c", cmd]).output().map(ExecOutput::from_output)
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
        let tag = new_exec_tag();
        let mut child = self
            .tagged_exec(&tag)
            .args(["--workdir", &self.workdir, &self.name])
            // Killing the host-side podman client doesn't stop the process
            // in the container: on timeout or cancel the tagged processes
            // are killed, and `timeout` bounds them in case that fails.
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
                    self.kill_tagged(&tag).await;
                    Err(ExecError::TimedOut(timeout))
                }
            },
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                self.kill_tagged(&tag).await;
                Err(ExecError::Cancelled)
            }
        }
    }

    /// A long-running process in the container that talks over its stdin
    /// and stdout (an MCP server): `podman exec -i` with these environment
    /// variables, in the project directory. The caller pipes and spawns it.
    pub fn interactive(&self, argv: &[&str], env: &[(String, String)]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.podman);
        cmd.arg("exec").arg("-i");
        // `--env NAME` takes the value from podman's own environment, so a
        // token never appears on a command line (`ps` would show it).
        for (k, v) in env {
            cmd.arg("--env").arg(k).env(k, v);
        }
        cmd.args(["--workdir", &self.workdir, &self.name]).args(argv);
        cmd
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
        let tag = new_exec_tag();
        let mut cmd = self.tagged_exec(&tag);
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
        let result = tokio::select! {
            result = tokio::time::timeout(timeout, work) => match result {
                Ok((stdout, stderr, Ok(status))) => Ok(RawOutput { stdout, stderr, code: status.code() }),
                Ok((_, _, Err(e))) => Err(ExecError::Wait(e)),
                Err(_) => Err(ExecError::TimedOut(timeout)),
            },
            _ = cancel.cancelled() => Err(ExecError::Cancelled),
        };
        if matches!(result, Err(ExecError::TimedOut(_) | ExecError::Cancelled)) {
            // `work` holds the child; dropping it kills the podman client.
            self.kill_tagged(&tag).await;
        }
        result
    }

    /// `podman exec` with the build-job variables and `SYL_EXEC=<tag>` in
    /// the environment.
    fn tagged_exec(&self, tag: &str) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.podman);
        cmd.arg("exec");
        for e in self.resources().job_env() {
            cmd.arg("--env").arg(e);
        }
        cmd.arg("--env").arg(format!("{EXEC_TAG}={tag}"));
        cmd
    }

    /// Kill every process an exec tagged `tag` started. Bounded, so a stuck
    /// podman can't hold up the caller for long.
    async fn kill_tagged(&self, tag: &str) {
        let run = tokio::process::Command::new(&self.podman)
            .args(["exec", &self.name, "/bin/sh", "-c", &kill_tagged_script(tag)])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status();
        let _ = tokio::time::timeout(Duration::from_secs(10), run).await;
    }

    /// Out-of-memory kills in the container since it started.
    fn read_oom_kills(&self) -> Option<u64> {
        let out = self.podman().args(["exec", &self.name, "/bin/sh", "-c", OOM_EVENTS_CMD]).output().ok()?;
        parse_oom_kills(&String::from_utf8_lossy(&out.stdout))
    }

    /// Processes the memory limit killed since the last time this was asked
    /// (or the container started). 0 when that can't be told.
    pub async fn new_oom_kills(&self) -> u64 {
        let run = tokio::process::Command::new(&self.podman)
            .args(["exec", &self.name, "/bin/sh", "-c", OOM_EVENTS_CMD])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(10), run).await else {
            return 0;
        };
        let Some(now) = parse_oom_kills(&String::from_utf8_lossy(&out.stdout)) else {
            return 0;
        };
        let before = self.oom_seen.swap(now, std::sync::atomic::Ordering::Relaxed);
        if before == u64::MAX { 0 } else { now.saturating_sub(before) }
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

/// Prints the container's cgroup memory events (v2, else v1).
const OOM_EVENTS_CMD: &str =
    "cat /sys/fs/cgroup/memory.events 2>/dev/null || cat /sys/fs/cgroup/memory/memory.oom_control 2>/dev/null";

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
    fn parses_memory_sizes() {
        assert_eq!(parse_size_mb("4g"), Some(4096));
        assert_eq!(parse_size_mb("4GB"), Some(4096));
        assert_eq!(parse_size_mb("512m"), Some(512));
        assert_eq!(parse_size_mb("1.5g"), Some(1536));
        assert_eq!(parse_size_mb("2147483648"), Some(2048));
        assert_eq!(parse_size_mb("auto"), None);
        assert_eq!(parse_size_mb("0g"), None);
        assert_eq!(parse_size_mb(""), None);
    }

    fn opts(memory: &str, cpus: Option<&str>) -> ContainerOptions {
        ContainerOptions { network: None, memory: memory.into(), cpus: cpus.map(Into::into) }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn auto_memory_is_a_share_of_podmans() {
        // podman's VM on a Mac: 4 GiB, 5 CPUs.
        let r = Resources::plan(&opts("auto", None), Some((4 * GIB, 5)));
        assert_eq!(r, Resources { memory_mb: 3072, cpus: 5, jobs: 3 });
        // A bigger VM: jobs follow the CPUs once memory allows.
        let r = Resources::plan(&opts("auto", None), Some((16 * GIB, 8)));
        assert_eq!(r, Resources { memory_mb: 12288, cpus: 8, jobs: 8 });
        // Without podman's figures, the old 2g.
        let r = Resources::plan(&opts("auto", None), None);
        assert_eq!(r, Resources { memory_mb: 2048, cpus: 2, jobs: 2 });
    }

    #[test]
    fn explicit_limits_win_and_jobs_fit_them() {
        let r = Resources::plan(&opts("2g", Some("4")), Some((16 * GIB, 8)));
        assert_eq!(r, Resources { memory_mb: 2048, cpus: 4, jobs: 2 });
        let r = Resources::plan(&opts("512m", Some("1.5")), Some((16 * GIB, 8)));
        assert_eq!(r, Resources { memory_mb: 512, cpus: 2, jobs: 1 });
        // More CPUs than podman has can't be used.
        let r = Resources::plan(&opts("64g", Some("32")), Some((16 * GIB, 8)));
        assert_eq!(r.cpus, 8);
        assert_eq!(r.jobs, 8);
    }

    #[test]
    fn job_env_names_the_usual_tools() {
        let r = Resources { memory_mb: 3072, cpus: 5, jobs: 3 };
        assert_eq!(r.job_env(), ["CARGO_BUILD_JOBS=3", "MAKEFLAGS=-j3", "CMAKE_BUILD_PARALLEL_LEVEL=3"]);
        assert_eq!(r.memory_text(), "3.0 GB");
        assert_eq!(r.jobs_text(), "3 build jobs");
        let small = Resources { memory_mb: 256, cpus: 1, jobs: 1 };
        assert_eq!((small.memory_text().as_str(), small.jobs_text().as_str()), ("256 MB", "1 build job"));
    }

    #[test]
    fn label_records_the_worked_out_memory() {
        let r = Resources { memory_mb: 3072, cpus: 5, jobs: 3 };
        assert_eq!(opts("auto", None).label(&r), "memory=3072m;cpus=;network=");
    }

    #[test]
    fn reads_oom_kill_counts() {
        let v2 = "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(parse_oom_kills(v2), Some(2));
        let v1 = "oom_kill_disable 0\nunder_oom 0\noom_kill 7\n";
        assert_eq!(parse_oom_kills(v1), Some(7));
        assert_eq!(parse_oom_kills(""), None);
    }

    /// Runs the script for real on the host, where there is a /proc: a
    /// tagged process in its own session dies, an untagged one lives.
    #[cfg(target_os = "linux")]
    #[test]
    fn kill_script_kills_tagged_processes_even_in_another_session() {
        use std::process::Command;
        let tag = new_exec_tag();
        let mut tagged = Command::new("setsid")
            .args(["sleep", "30"])
            .env(EXEC_TAG, &tag)
            .spawn()
            .unwrap();
        let mut other = Command::new("sleep").arg("30").env(EXEC_TAG, "someone-else").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let st = Command::new("/bin/sh").args(["-c", &kill_tagged_script(&tag)]).status().unwrap();
        assert!(st.success());
        let status = tagged.wait().unwrap();
        assert!(!status.success(), "the tagged process should have been killed");
        assert!(other.try_wait().unwrap().is_none(), "the untagged one should still run");
        let _ = other.kill();
    }

    #[test]
    fn kill_script_matches_only_its_own_tag() {
        let s = kill_tagged_script("123-4-5");
        assert!(s.contains("grep -qxF 'SYL_EXEC=123-4-5'"));
        assert!(s.contains("kill -KILL"));
    }

    #[test]
    fn exec_error_text_skips_empty_streams() {
        let out = ExecOutput { stdout: "  ".into(), stderr: "boom\n".into(), code: Some(2) };
        assert_eq!(out.error_text(), "Error (Exit 2): boom");
    }
}
