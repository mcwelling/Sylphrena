//! Syl's containers: [urithiru] with Syl's profile.
//!
//! Urithiru owns the container lifecycle and command execution. What is
//! Syl's lives here: the container names, the image Syl's tools are baked
//! into, the read-only `.syl` mount, and the SSH and git setup.
use std::path::Path;

pub use urithiru::*;

/// Containers are named `syl-agent-<hash>`.
pub const NAME_PREFIX: &str = "syl-agent";
/// Image committed after the first cold start so later starts skip setup.
pub const BAKED_IMAGE: &str = "syl-agent-ready:latest";
/// Container name used by Syl versions before per-project names existed.
pub const LEGACY_CONTAINER_NAME: &str = "syl-agent";
/// Label recording the options a container was started with.
const OPTIONS_LABEL: &str = "syl.options";

/// Syl's container name for a project.
pub fn name_for(project_dir: &Path) -> String {
    urithiru::name_for(NAME_PREFIX, project_dir)
}

/// The container for `project_dir`, with Syl's profile.
pub fn for_project(project_dir: &Path) -> Container {
    let dir = project_dir.canonicalize().unwrap_or_else(|_| project_dir.to_path_buf());
    Container::new(&dir, profile(&dir))
}

/// Syl's profile for a (canonical) project directory.
pub fn profile(project_dir: &Path) -> Profile {
    let state_dir = project_dir.join(".syl");
    let mut p = Profile::new(NAME_PREFIX, state_dir.join("container-ready"));
    p.baked_image = BAKED_IMAGE.to_string();
    p.options_label = OPTIONS_LABEL.to_string();
    p.legacy_name = Some(LEGACY_CONTAINER_NAME.to_string());
    p.setup = Some(SETUP_TOOLS_CMD.to_string());
    // .syl/ is mounted read-only over the workspace copy so code running
    // in the container can't rewrite Syl's memory (injected into the
    // system prompt), workflows (run as commands), session or log.
    p.mounts.push(Mount::read_only(&state_dir, format!("{WORKDIR}/.syl")));

    let home = std::env::var("HOME").unwrap_or_default();
    let ssh_dir = Path::new(&home).join(".ssh");
    if !home.is_empty() && ssh_dir.exists() {
        p.mounts.push(Mount::read_only(&ssh_dir, "/root/.ssh"));
        p.after_start.push(SSH_SETUP_CMD.to_string());
    }
    // The host's git identity.
    for key in ["user.name", "user.email"] {
        let value = std::process::Command::new("git")
            .args(["config", "--global", key])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if !value.is_empty() {
            p.after_start.push(format!("git config --global {key} {}", shell_quote(&value)));
        }
    }
    p
}

/// Copies the read-only SSH keys (ssh refuses keys it doesn't own) and wraps
/// ssh so it uses the copied config and known_hosts.
const SSH_SETUP_CMD: &str = "bash -c '\
    cp -rp /root/.ssh/. /tmp/.ssh/ 2>/dev/null || true \
    && chown -R root:root /tmp/.ssh \
    && chmod 700 /tmp/.ssh \
    && find /tmp/.ssh -type f -exec chmod 600 {} \\; \
    && find /tmp/.ssh -name \"*.pub\" -exec chmod 644 {} \\; \
    && [ -f /tmp/.ssh/config ] || touch /tmp/.ssh/config \
    && ssh-keyscan -H github.com >> /tmp/.ssh/known_hosts 2>/dev/null \
    && printf \"#!/bin/sh\\nexec /usr/bin/ssh -F /tmp/.ssh/config -o UserKnownHostsFile=/tmp/.ssh/known_hosts \\\"\\$@\\\"\\n\" > /usr/local/bin/ssh \
    && chmod +x /usr/local/bin/ssh\
'";

/// Syl's tools, installed once and baked into [`BAKED_IMAGE`].
const SETUP_TOOLS_CMD: &str = concat!(
    "apt-get update -qq && ",
    "apt-get install -y --no-install-recommends ",
    "python3 perl jq poppler-utils chafa python3-pil ripgrep ",
    "git curl wget unzip build-essential pkg-config libssl-dev && ",
    // timg is arch-specific — install best-effort so it never blocks the chain
    "apt-get install -y timg 2>/dev/null || echo 'timg not available in repos, skipping' && ",
    // yq, for this machine's architecture (amd64 or arm64)
    "wget -qO /usr/local/bin/yq https://github.com/mikefarah/yq/releases/latest/download/yq_linux_$(dpkg --print-architecture) && ",
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
    fn keeps_the_names_existing_containers_have() {
        let p = profile(Path::new("/Users/me/code/a"));
        assert_eq!(p.name_prefix, "syl-agent");
        assert_eq!(p.baked_image, "syl-agent-ready:latest");
        assert_eq!(p.options_label, "syl.options");
        assert_eq!(p.marker, Path::new("/Users/me/code/a/.syl/container-ready"));
        assert_eq!(p.legacy_name.as_deref(), Some("syl-agent"));
        assert_eq!(p.mounts[0], Mount::read_only("/Users/me/code/a/.syl", "/workspace/.syl"));
    }

    #[test]
    fn yq_matches_the_machine() {
        assert!(SETUP_TOOLS_CMD.contains("yq_linux_$(dpkg --print-architecture)"));
        assert!(!SETUP_TOOLS_CMD.contains("amd64"));
    }
}
