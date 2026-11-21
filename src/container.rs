/// Podman container lifecycle management.
/// Mirrors start-or-resume-container, cold-start-container, container-alive?, etc. from syl.bb.
use std::path::Path;
use std::process::Command;

use crate::config::{container_name, AppConfig, BAKED_IMAGE, LEGACY_CONTAINER_NAME};
use crate::util::shell_quote;

// ---------------------------------------------------------------------------
// Marker file helpers
// ---------------------------------------------------------------------------

/// Read the container ID saved in the marker file.
pub fn read_container_marker(session_dir: &Path) -> Option<String> {
    let path = session_dir.join("container-ready");
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Write the running container's ID to the marker file.
pub fn write_container_marker(session_dir: &Path) -> anyhow::Result<()> {
    let id = podman_inspect_id()?;
    let path = session_dir.join("container-ready");
    std::fs::write(path, id)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

fn podman_inspect_id() -> anyhow::Result<String> {
    let out = Command::new("podman")
        .args(["inspect", "--format", "{{.Id}}", container_name()])
        .output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        anyhow::bail!("podman inspect failed: {}", String::from_utf8_lossy(&out.stderr))
    }
}

/// Returns `true` if the container is in Running state.
pub fn container_alive() -> bool {
    let out = Command::new("podman")
        .args(["inspect", "-f", "{{.State.Running}}", container_name()])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().contains("true"),
        Err(_) => false,
    }
}

fn baked_image_exists() -> bool {
    let out = Command::new("podman")
        .args(["image", "exists", BAKED_IMAGE])
        .output();
    matches!(out, Ok(o) if o.status.success())
}

/// Run a shell command (via `/bin/sh -c`) on the host, capture output.
/// Used for podman management commands (not tool execution).
fn sh(cmd: &str) -> anyhow::Result<()> {
    let status = Command::new("sh").args(["-c", cmd]).status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("command failed (exit {}): {}", status.code().unwrap_or(-1), cmd)
    }
}

fn sh_output(cmd: &str) -> anyhow::Result<String> {
    let out = Command::new("sh").args(["-c", cmd]).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Container setup
// ---------------------------------------------------------------------------

const SETUP_TOOLS_CMD: &str = concat!(
    "apt-get update -qq && ",
    "apt-get install -y --no-install-recommends ",
    "python3 perl jq poppler-utils chafa python3-pil ",
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

fn configure_git_and_ssh(config: &AppConfig) {
    // Copy SSH keys and set up SSH wrapper with correct permissions and known_hosts
    let home = std::env::var("HOME").unwrap_or_default();
    let ssh_dir = format!("{home}/.ssh");
    if Path::new(&ssh_dir).exists() {
        let _ = exec_command_raw(
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
            config,
        );
    }

    // Propagate host git identity
    let name = sh_output("git config --global user.name").unwrap_or_default();
    let email = sh_output("git config --global user.email").unwrap_or_default();
    if !name.is_empty() {
        let _ = exec_command_raw(
            &format!("git config --global user.name {}", shell_quote(&name)),
            config,
        );
    }
    if !email.is_empty() {
        let _ = exec_command_raw(
            &format!("git config --global user.email {}", shell_quote(&email)),
            config,
        );
    }
}

/// Check that /workspace is a live bind-mount inside the container.
fn workspace_mounted(_config: &AppConfig) -> bool {
    let out = Command::new("podman")
        .args(["exec", container_name(), "mountpoint", "-q", "/workspace"])
        .output();
    matches!(out, Ok(o) if o.status.success())
}

pub fn cold_start(config: &AppConfig) -> anyhow::Result<()> {
    let home = std::env::var("HOME").unwrap_or_default();
    let ssh_dir = format!("{home}/.ssh");
    let has_bake = baked_image_exists();
    let base = if has_bake { BAKED_IMAGE } else { "mcr.microsoft.com/devcontainers/base:ubuntu" };
    let project_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();

    println!("Summoning a more powerful Syl in: {project_dir}");
    if has_bake {
        println!("[Syl Setup] Using cached image — skipping dependency install.");
    }

    let ssh_vol = if Path::new(&ssh_dir).exists() {
        format!("-v {}:/root/.ssh:ro ", shell_quote(&ssh_dir))
    } else {
        String::new()
    };

    // Mount .syl/ read-only over the workspace copy so code running in the
    // container can't rewrite Syl's own memory (injected into the system
    // prompt), workflows (run as commands), session or log.
    let syl_dir = format!("{project_dir}/.syl");
    let _ = std::fs::create_dir_all(&syl_dir);
    let syl_vol = format!("-v {}:/workspace/.syl:ro ", shell_quote(&syl_dir));

    let name = container_name();
    sh(&format!(
        "podman run -d --name {name} \
         --memory=2g \
         -e TERM=xterm-256color \
         -e TERM_PROGRAM={term} \
         -v {ws}:/workspace:rw \
         {syl_vol}\
         {ssh_vol}\
         --workdir /workspace \
         -it {base} /bin/bash",
        term = shell_quote(&term_program),
        ws = shell_quote(&project_dir),
    ))?;

    configure_git_and_ssh(config);

    if !has_bake {
        println!("[Syl Setup] Installing tools...");
        exec_command_raw(SETUP_TOOLS_CMD, config)
            .map_err(|e| anyhow::anyhow!("tool install failed: {}", e))?;
        println!("[Syl Setup] Baking image for fast restarts...");
        sh(&format!("podman commit {} {BAKED_IMAGE}", container_name()))?;
        println!("[Syl Setup] Image cached as {BAKED_IMAGE}.");
    }

    Ok(())
}

/// Remove a container left by an older Syl that used the single shared name
/// `syl-agent`, but only when its /workspace is *this* project, so another
/// project's session is never touched.
fn remove_legacy_container() {
    let out = Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{range .Mounts}}{{if eq .Destination \"/workspace\"}}{{.Source}}{{end}}{{end}}",
            LEGACY_CONTAINER_NAME,
        ])
        .output();
    let Ok(out) = out else { return };
    if !out.status.success() {
        return;
    }
    let source = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let here = std::env::current_dir().unwrap_or_default();
    let here = here.canonicalize().unwrap_or(here);
    if !source.is_empty() && Path::new(&source) == here.as_path() {
        let _ = Command::new("podman").args(["rm", "-f", LEGACY_CONTAINER_NAME]).output();
    }
}

pub fn nuke_container(hard: bool) {
    let _ = sh(&format!("podman rm -f {}", container_name()));
    if hard {
        let _ = sh(&format!("podman rmi -f {BAKED_IMAGE}"));
    }
}

// ---------------------------------------------------------------------------
// Start or resume
// ---------------------------------------------------------------------------

pub fn start_or_resume(config: &AppConfig, session_dir: &Path) -> anyhow::Result<()> {
    let saved_id = read_container_marker(session_dir);
    let fresh = config.fresh;

    let live_id: Option<String> = if !fresh && saved_id.is_some() {
        podman_inspect_id().ok()
    } else {
        None
    };

    let resume = !fresh
        && saved_id.is_some()
        && live_id.is_some()
        && saved_id == live_id;

    if resume {
        println!("Resuming container...");
        let _ = sh(&format!("podman start {}", container_name()));
        configure_git_and_ssh(config);

        if !workspace_mounted(config) {
            println!("[Syl] Workspace bind mount lost after warm start — rebuilding container...");
            nuke_container(false);
            cold_start(config)?;
        }
    } else {
        remove_legacy_container();
        nuke_container(fresh);
        cold_start(config)?;
    }

    write_container_marker(session_dir)?;
    Ok(())
}

pub fn ensure_container(config: &AppConfig, session_dir: &Path) -> anyhow::Result<()> {
    if !container_alive() {
        println!("\n[Syl] Container is gone — resummoning...");
        start_or_resume(config, session_dir)?;
        println!("[Syl] Container restored. Continuing.\n");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Command execution inside the container
// ---------------------------------------------------------------------------

/// Run a raw bash command inside the container. Returns (stdout+stderr, is_error).
/// This is the synchronous path used by the setup helpers.
pub fn exec_command_raw(cmd: &str, _config: &AppConfig) -> anyhow::Result<String> {
    let out = Command::new("podman")
        .args(["exec", container_name(), "/bin/bash", "-c", cmd])
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(stdout)
    } else {
        let code = out.status.code().unwrap_or(-1);
        let combined = format!("Error (Exit {code}): {}",
            [stdout.trim(), stderr.trim()]
                .iter()
                .filter(|s| !s.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"));
        anyhow::bail!("{}", combined)
    }
}

// ---------------------------------------------------------------------------
// Container stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ContainerStats {
    pub cpu_percent: f64,
    pub mem_usage_mb: f64,
    pub mem_limit_mb: f64,
}

/// Non-blocking stats poll. Returns None if the container is stopped or podman fails.
pub fn poll_stats() -> Option<ContainerStats> {
    let out = Command::new("podman")
        .args(["stats", "--no-stream", "--format", "json", container_name()])
        .output()
        .ok()?;
    if !out.status.success() { return None; }

    let text = String::from_utf8_lossy(&out.stdout);
    let arr: serde_json::Value = serde_json::from_str(&text).ok()?;
    let item = arr.get(0)?;

    let cpu = item["CPUPerc"].as_str()
        .and_then(|s| s.trim_end_matches('%').parse::<f64>().ok())
        .unwrap_or(0.0);

    // "MemUsage" is a string like "123.4MB / 2GB"
    let mem_str = item["MemUsage"].as_str().unwrap_or("");
    let parts: Vec<&str> = mem_str.split('/').collect();
    let mem_use = parse_mem_str(parts.first().copied().unwrap_or(""));
    let mem_lim = parse_mem_str(parts.get(1).copied().unwrap_or(""));

    Some(ContainerStats {
        cpu_percent:  cpu,
        mem_usage_mb: mem_use,
        mem_limit_mb: mem_lim,
    })
}

fn parse_mem_str(s: &str) -> f64 {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("GB") {
        n.trim().parse::<f64>().unwrap_or(0.0) * 1024.0
    } else if let Some(n) = s.strip_suffix("MB") {
        n.trim().parse::<f64>().unwrap_or(0.0)
    } else if let Some(n) = s.strip_suffix("kB") {
        n.trim().parse::<f64>().unwrap_or(0.0) / 1024.0
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Reconnect
// ---------------------------------------------------------------------------

/// Attempt to reconnect to a container that has died unexpectedly.
///
/// This is a named path so the failure mode is explicit rather than silent.
/// Phase 4 will call this from `ensure_container` when the container
/// disappears mid-session (e.g. host sleep/wake or OOM kill).
pub fn reconnect(config: &AppConfig, session_dir: &Path) -> anyhow::Result<()> {
    tracing::warn!("container gone — attempting reconnect");
    nuke_container(false);
    cold_start(config)?;
    write_container_marker(session_dir)?;
    tracing::info!("container reconnected");
    Ok(())
}
