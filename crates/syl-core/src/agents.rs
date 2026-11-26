//! Subagents: what the `task` tool can start.
//!
//! A subagent runs in a child session with fresh context, the parent's
//! container and permission rules, a restricted set of tools, and its own
//! request and token budgets, and only its final reply goes back to the
//! parent. Subagents can't start subagents (`task` is never theirs), so the
//! depth is one.
//!
//! Definitions are markdown with front matter in `.syl/agents/` (the
//! project's) or `~/.syl/agents/`, the shape opencode and Claude Code use:
//!
//! ```markdown
//! ---
//! description: Reviews a diff for error handling.
//! model: claude-haiku-5-5   # optional; default: the parent's model
//! tools: read_file, grep     # optional; default: every tool but task
//! max_requests: 20           # optional; default 25
//! token_budget: 200000       # optional; default 400000
//! ---
//! You review diffs. ...
//! ```
//!
//! The file name is the agent's name. A project's definition replaces a
//! built-in or home one of the same name. Like custom commands, a project's
//! definitions come with the project; they only shape prompts and narrow
//! tools, and every call still goes through the permission rules.
use std::path::{Path, PathBuf};

use crate::commands::{contained, field, first_line, front_matter, home, is_link};

/// Model requests a subagent may make, by default.
pub const DEFAULT_MAX_REQUESTS: u32 = 25;
/// Tokens a subagent may use over all its requests, by default: input,
/// cache writes and output. Cache reads aren't counted: every request
/// reads the whole context back, and they cost a tenth as much.
pub const DEFAULT_TOKEN_BUDGET: u64 = 400_000;
/// The most a definition may set.
const MAX_REQUESTS_CAP: u64 = 200;
const TOKEN_BUDGET_CAP: u64 = 5_000_000;

/// The tools there are, besides MCP tools (`server__tool`).
const KNOWN_TOOLS: &[&str] = &[
    "run_bash", "read_file", "edit_file", "write_file", "grep", "glob", "ls", "memory_read", "memory_write",
    "workflow_write", "workflow_delete", "show_image", "fetch", "skill",
];
/// Subagents that run at once when one response starts several.
pub const MAX_PARALLEL: usize = 4;

/// Tools that change the workspace: an agent with any of them is
/// checkpointed before it runs, like the tools themselves.
const WRITING_TOOLS: &[&str] = &["run_bash", "edit_file", "write_file", "memory_write", "workflow_write", "workflow_delete"];

/// Tools that start a subagent: never a subagent's own (depth one).
pub const STARTS_SUBAGENTS: &[&str] = &["task", "dispatch_agent"];

/// The model the `fetch` agent uses.
pub const FETCH_MODEL: &str = "claude-haiku-4-5-20251001";

/// One kind of subagent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// None: the parent's model.
    pub model: Option<String>,
    /// None: every tool the parent has, except `task`.
    pub tools: Option<Vec<String>>,
    pub max_requests: u32,
    pub token_budget: u64,
    /// The system prompt's instructions.
    pub prompt: String,
    /// Where it was defined; None for a built-in.
    pub path: Option<PathBuf>,
}

impl AgentDef {
    /// Whether it can have `name`. `task` is never a subagent's.
    pub fn allows(&self, name: &str) -> bool {
        !STARTS_SUBAGENTS.contains(&name) && self.tools.as_ref().is_none_or(|t| t.iter().any(|x| x == name))
    }

    /// Whether any of its tools can change the workspace (MCP tools and
    /// unrestricted agents count as able to).
    pub fn can_write(&self) -> bool {
        match &self.tools {
            None => true,
            Some(t) => t.iter().any(|x| WRITING_TOOLS.contains(&x.as_str()) || x.contains("__")),
        }
    }
}

fn builtin(name: &str, description: &str, tools: Option<&[&str]>, model: Option<&str>, prompt: &str) -> AgentDef {
    AgentDef {
        name: name.to_string(),
        description: description.to_string(),
        model: model.map(str::to_string),
        tools: tools.map(|t| t.iter().map(|s| s.to_string()).collect()),
        max_requests: DEFAULT_MAX_REQUESTS,
        token_budget: DEFAULT_TOKEN_BUDGET,
        prompt: prompt.to_string(),
        path: None,
    }
}

/// The agents every project has.
pub fn builtins() -> Vec<AgentDef> {
    vec![
        builtin(
            "explore",
            "Read-only search of the project: finds files and code, reads them, and reports what it found \
             with paths and line numbers. Use it for questions that need many searches or reads, to keep \
             them out of your own context.",
            Some(&["read_file", "grep", "glob", "ls", "memory_read"]),
            None,
            "You explore a codebase to answer one question for another agent, who will act on your answer. \
             You can only read. Search widely first (grep, glob), then read just the parts that matter. \
             Reply with what you found: the answer first, then the evidence as `path:line` references with \
             short quotes. Say what you looked for and didn't find. Don't propose changes unless asked.",
        ),
        builtin(
            "general",
            "A general-purpose agent for a self-contained task: it can read, run commands and change files, \
             and reports what it did. Use it for work you can describe completely in the prompt.",
            None,
            None,
            "You carry out one self-contained task for another agent, in the same workspace. Do the task, \
             check that it worked, and reply with what you did and found: changed files, commands run and \
             their results, and anything left undone or uncertain. Be brief; the other agent reads only \
             your reply.",
        ),
        builtin(
            "fetch",
            "Fetches web pages and answers a question about them. Give it the URL and exactly what to \
             find, summarise or extract.",
            Some(&["fetch"]),
            Some(FETCH_MODEL),
            "You are a research assistant. Fetch the page you're given (and only pages it links to, if \
             needed), then return only what was asked for: no preamble, no commentary, no apology. Output \
             must match the requested format exactly.",
        ),
    ]
}

fn agent_dirs(project: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project.join(".syl/agents")];
    dirs.extend(home().map(|h| h.join(".syl/agents")));
    dirs
}

/// `read_file, grep` or `[read_file, grep]` to names.
fn tool_list(v: &str) -> Vec<String> {
    v.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split([',', ' '])
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\''))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A field's value without a trailing ` # comment`.
fn uncomment(v: &str) -> String {
    match v.find(" #") {
        Some(i) => v[..i].trim().to_string(),
        None => v.trim().to_string(),
    }
}

/// `key:` followed by `- item` lines in the front matter (a YAML block
/// list), which `front_matter` leaves out.
fn block_list(text: &str, key: &str) -> Option<Vec<String>> {
    let header = text.strip_prefix("---")?.split("\n---").next()?;
    let mut lines = header.lines().skip_while(|l| l.trim_end() != format!("{key}:"));
    lines.next()?;
    let items: Vec<String> = lines
        .map_while(|l| l.trim_start().strip_prefix("- ").filter(|_| l.starts_with([' ', '\t', '-'])))
        .map(|i| uncomment(i).trim_matches(|c| c == '"' || c == '\'').to_string())
        .filter(|i| !i.is_empty())
        .collect();
    (!items.is_empty()).then_some(items)
}

/// A definition from a file's text; `problems` gets what was ignored.
pub fn parse(name: &str, text: &str, path: Option<PathBuf>, problems: &mut Vec<String>) -> AgentDef {
    let (fields, body) = front_matter(text);
    let has = |key: &str| fields.iter().any(|(k, _)| k == key);
    let num = |key: &str, default: u64, cap: u64, problems: &mut Vec<String>| match field(&fields, key).map(|v| uncomment(&v)) {
        None => default,
        Some(v) => match v.parse::<u64>().ok().filter(|n| *n > 0) {
            Some(n) if n > cap => {
                problems.push(format!("agent {name}: `{key}: {n}` is over {cap}; using {cap}"));
                cap
            }
            Some(n) => n,
            None => {
                problems.push(format!("agent {name}: `{key}: {v}` isn't a positive number; using {default}"));
                default
            }
        },
    };
    // A `tools` line that is there but empty or unreadable gives no tools,
    // never every tool: a reviewer meant to read mustn't get run_bash.
    let tools = if has("tools") {
        let mut t = block_list(text, "tools").unwrap_or_else(|| tool_list(&uncomment(&field(&fields, "tools").unwrap_or_default())));
        if t.is_empty() {
            problems.push(format!("agent {name}: `tools` lists nothing it can read, so it has no tools"));
        }
        if t.iter().any(|x| STARTS_SUBAGENTS.contains(&x.as_str())) {
            problems.push(format!("agent {name}: subagents can't start subagents, so task and dispatch_agent were left out"));
            t.retain(|x| !STARTS_SUBAGENTS.contains(&x.as_str()));
        }
        let unknown: Vec<&String> = t.iter().filter(|x| !KNOWN_TOOLS.contains(&x.as_str()) && !x.contains("__")).collect();
        if !unknown.is_empty() {
            let list: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
            problems.push(format!("agent {name}: no tool called {}", list.join(", ")));
        }
        Some(t)
    } else {
        None
    };
    if has("max_tokens") {
        problems.push(format!("agent {name}: `max_tokens` isn't read; the budget over all its requests is `token_budget`"));
    }
    let model = field(&fields, "model").map(|m| uncomment(&m)).filter(|m| !m.is_empty() && m != "inherit");
    AgentDef {
        name: name.to_string(),
        description: field(&fields, "description").unwrap_or_else(|| first_line(&body)),
        model,
        tools,
        max_requests: num("max_requests", DEFAULT_MAX_REQUESTS as u64, MAX_REQUESTS_CAP, problems) as u32,
        token_budget: num("token_budget", DEFAULT_TOKEN_BUDGET, TOKEN_BUDGET_CAP, problems),
        prompt: body.trim().to_string(),
        path,
    }
}

/// Every agent: the project's, then home's, then the built-ins; a name
/// already seen is skipped. Symbolic links aren't followed (a cloned
/// project could ship `x.md -> ~/.ssh/id_rsa`). Returns what was ignored
/// too.
pub fn list_agents(project: &Path) -> (Vec<AgentDef>, Vec<String>) {
    let mut out: Vec<AgentDef> = Vec::new();
    let mut problems = Vec::new();
    let home = home();
    for dir in agent_dirs(project) {
        let base = if dir.starts_with(project) { project.to_path_buf() } else { home.clone().unwrap_or_default() };
        if !contained(&dir, &base) {
            continue;
        }
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let mut paths: Vec<PathBuf> = read.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.extension().is_none_or(|e| e != "md") || is_link(&path) {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
            if !crate::util::is_safe_key(&name) || out.iter().any(|a| a.name == name) {
                continue;
            }
            let Ok(text) = crate::util::read_regular(&path) else { continue };
            out.push(parse(&name, &text, Some(path), &mut problems));
        }
    }
    for b in builtins() {
        if !out.iter().any(|a| a.name == b.name) {
            out.push(b);
        }
    }
    (out, problems)
}

/// The agent called `name`.
pub fn find(project: &Path, name: &str) -> Option<AgentDef> {
    list_agents(project).0.into_iter().find(|a| a.name == name)
}

/// The `task` tool, listing the agents it can start.
pub fn task_tool(agents: &[AgentDef]) -> serde_json::Value {
    let list: Vec<String> = agents.iter().map(|a| format!("- {}: {}", a.name, a.description)).collect();
    serde_json::json!({
        "name": "task",
        "description": format!(
            "Start a subagent on a self-contained task. It works in a fresh context (it sees none of this \
             conversation, only your prompt), in the same workspace and under the same permission rules, and \
             only its final reply comes back to you. Use it to keep long searches, many reads or a side task \
             out of your own context. Write the prompt so it stands alone: the goal, what you already know, \
             and what to report. Several task calls in one response run at the same time (up to {MAX_PARALLEL}); \
             don't run two that change the same files at once.\n\nAgents:\n{}",
            list.join("\n")
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "agent": {"type": "string", "enum": agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>(), "description": "Which agent."},
                "description": {"type": "string", "description": "A few words on what it's doing, shown to the user."},
                "prompt": {"type": "string", "description": "The complete task. The agent sees only this."}
            },
            "required": ["agent", "description", "prompt"]
        }
    })
}

/// The system prompt's text for a subagent: its instructions, and how it
/// relates to the agent that started it.
pub fn system_text(def: &AgentDef) -> String {
    format!(
        "You are a subagent of Syl, an AI coding agent, started for one task. You work in Syl's \
         container: the project is at /workspace. The agent that started you sees only your final \
         reply, so make it complete and self-contained. You can't ask the user questions; if something \
         is unclear, say so in your reply. You have a budget of {} model requests; near the end you'll be \
         asked to wrap up.\n\n### YOUR INSTRUCTIONS ({})\n{}",
        def.max_requests, def.name, def.prompt
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_definition_parses() {
        let mut problems = Vec::new();
        let d = parse(
            "review",
            "---\ndescription: Reviews diffs.\nmodel: claude-haiku-5-5   # cheap\ntools: [read_file, grep, task]  # read-only\nmax_requests: 5 # few\ntoken_budget: nope\n---\nYou review.\n",
            None,
            &mut problems,
        );
        assert_eq!(d.description, "Reviews diffs.");
        assert_eq!(d.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(d.tools, Some(vec!["read_file".to_string(), "grep".to_string()]));
        assert_eq!(d.max_requests, 5);
        assert_eq!(d.token_budget, DEFAULT_TOKEN_BUDGET);
        assert_eq!(d.prompt, "You review.");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(!d.allows("task") && !d.allows("run_bash") && d.allows("grep"));
        assert!(!d.can_write());
    }

    #[test]
    fn a_block_list_or_empty_tools_never_means_every_tool() {
        let mut problems = Vec::new();
        let d = parse("r", "---\ntools:\n  - read_file\n  - grep   # search\nmax_requests: 3\n---\nP", None, &mut problems);
        assert_eq!(d.tools.as_deref(), Some(&["read_file".to_string(), "grep".to_string()][..]));
        assert_eq!(d.max_requests, 3);
        assert!(problems.is_empty(), "{problems:?}");
        let d = parse("r", "---\ntools: \"\"\n---\nP", None, &mut problems);
        assert_eq!(d.tools, Some(vec![]));
        assert!(!d.allows("run_bash") && !d.can_write());
        assert!(problems.iter().any(|p| p.contains("no tools")));
    }

    #[test]
    fn unknown_tools_old_fields_and_huge_budgets_are_reported() {
        let mut problems = Vec::new();
        let d = parse("r", "---\ntools: Read, Grep, gh__search\nmax_tokens: 4096\nmax_requests: 100000\n---\nP", None, &mut problems);
        assert_eq!(d.max_requests, 200);
        assert!(problems.iter().any(|p| p.contains("no tool called Read, Grep")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("token_budget")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("over 200")), "{problems:?}");
    }

    #[test]
    fn defaults_and_inherit() {
        let d = parse("x", "Just a prompt.\n", None, &mut Vec::new());
        assert_eq!(d.description, "Just a prompt.");
        assert_eq!(d.model, None);
        assert!(d.allows("run_bash") && !d.allows("task"));
        assert!(d.can_write());
        let d = parse("x", "---\nmodel: inherit\ntools: read_file grep\n---\nP", None, &mut Vec::new());
        assert_eq!(d.model, None);
        assert_eq!(d.tools.unwrap(), ["read_file", "grep"]);
    }

    #[test]
    fn project_definitions_replace_builtins_and_links_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".syl/agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("explore.md"), "---\ndescription: Mine.\n---\nP").unwrap();
        std::fs::write(tmp.path().join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("secret"), dir.join("leak.md")).unwrap();
        let (agents, _) = list_agents(tmp.path());
        let explore = agents.iter().find(|a| a.name == "explore").unwrap();
        assert_eq!(explore.description, "Mine.");
        assert!(agents.iter().all(|a| a.name != "leak"));
        assert!(agents.iter().any(|a| a.name == "general") && agents.iter().any(|a| a.name == "fetch"));
    }

    #[test]
    fn the_task_tool_lists_the_agents() {
        let t = task_tool(&builtins());
        assert_eq!(t["name"], "task");
        assert!(t["description"].as_str().unwrap().contains("- explore:"));
        assert_eq!(t["input_schema"]["properties"]["agent"]["enum"], serde_json::json!(["explore", "general", "fetch"]));
    }
}
