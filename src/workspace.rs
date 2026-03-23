use std::path::Path;
use std::process::Command;

/// Gather workspace context for the system prompt: file tree, git branch, git status.
///
/// Runs on the host (not inside the container). Mirrors `gather-workspace-context` in syl.bb.
/// Returns a formatted markdown string beginning with a newline, ready to embed in the system
/// prompt between the instruction text and the memory section.
pub fn gather_workspace_context(project_dir: &Path) -> String {
    let tree = run_sh(
        "find . -maxdepth 2 \
         -not -path './.git/*' \
         -not -path './.syl/*' \
         -not -path './.idea/*' \
         -not -path './.clj-kondo/*' \
         -not -path './.lsp/*' \
         | sort | head -50",
        project_dir,
    )
    .unwrap_or_else(|| "(could not list files)".to_string());

    let git_branch = run_cmd("git", &["branch", "--show-current"], project_dir)
        .unwrap_or_default();

    let git_status = run_cmd("git", &["status", "--short"], project_dir)
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str("\n### WORKSPACE STATE (auto-injected at session start)\n");
    out.push_str(&format!("Project: {}\n", project_dir.display()));
    if !git_branch.trim().is_empty() {
        out.push_str(&format!("Branch: {}\n", git_branch.trim()));
    }
    out.push_str("Files:\n```\n");
    out.push_str(tree.trim());
    out.push_str("\n```\n");
    if !git_status.trim().is_empty() {
        out.push_str("Git status:\n```\n");
        out.push_str(git_status.trim());
        out.push_str("\n```\n");
    }
    out
}

/// Run a shell command via `sh -c` in the given directory.
/// Returns stdout on success (exit 0 or non-zero), None if the process couldn't spawn.
fn run_sh(cmd: &str, dir: &Path) -> Option<String> {
    let out = Command::new("sh")
        .args(["-c", cmd])
        .current_dir(dir)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Run a simple command (no shell) in the given directory.
/// Returns stdout on exit 0, empty string on non-zero, None if spawn failed.
fn run_cmd(prog: &str, args: &[&str], dir: &Path) -> Option<String> {
    let out = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Some(String::new())
    }
}
