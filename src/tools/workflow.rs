use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::util::is_safe_key;

fn bad_key(key: &str) -> String {
    format!("Error: invalid workflow name '{}'. Use letters, digits, '-', '_' or '.' (no '..', no leading '.').", key)
}

static STEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-*]\s+(.+?):\s*`(.+?)`\s*$").unwrap());

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowStep {
    pub label:   String,
    pub command: String,
}

#[derive(Debug, Clone)]
pub struct Workflow {
    pub name:        String,
    pub description: Option<String>,
    pub steps:       Vec<WorkflowStep>,
}

#[derive(Debug, Clone)]
pub struct WorkflowEntry {
    pub key:  String,
    pub file: String,
}

/// Parse a workflow markdown string.
/// Returns None if there are no steps.
pub fn parse_workflow(content: &str) -> Option<Workflow> {
    let step_re = &*STEP_RE;
    let lines: Vec<&str> = content.lines().collect();

    let name = lines.iter()
        .filter(|l| l.trim().starts_with("# "))
        .map(|l| l.trim().trim_start_matches('#').trim().to_string())
        .next();

    let steps: Vec<WorkflowStep> = lines.iter()
        .filter_map(|l| {
            step_re.captures(l).map(|c| WorkflowStep {
                label:   c[1].trim().to_string(),
                command: c[2].trim().to_string(),
            })
        })
        .collect();

    if steps.is_empty() {
        return None;
    }

    // Description: non-blank, non-heading, non-step lines before the first step
    let first_step_line = lines.iter().position(|l| step_re.is_match(l)).unwrap_or(lines.len());
    let desc: String = lines[..first_step_line]
        .iter()
        .skip_while(|l| l.trim().is_empty() || l.trim().starts_with('#'))
        .filter(|l| !l.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();

    Some(Workflow {
        name:        name.unwrap_or_else(|| "unnamed".to_string()),
        description: if desc.is_empty() { None } else { Some(desc) },
        steps,
    })
}

/// List workflow files. Returns sorted entries.
pub fn list_workflows(workflow_dir: &Path) -> Vec<WorkflowEntry> {
    let entries = match fs::read_dir(workflow_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };
    let mut files: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".md"))
        .collect();
    files.sort();
    files.into_iter().map(|fname| WorkflowEntry {
        key:  fname.trim_end_matches(".md").to_string(),
        file: fname,
    }).collect()
}

/// Load and parse a workflow by key. Returns None if missing or invalid.
pub fn load_workflow(workflow_dir: &Path, key: &str) -> Option<Workflow> {
    if !is_safe_key(key) {
        return None;
    }
    let path = workflow_dir.join(format!("{}.md", key));
    let content = fs::read_to_string(path).ok()?;
    parse_workflow(&content)
}

/// Create a new workflow. Refuses to overwrite an existing one.
pub fn workflow_write(workflow_dir: &Path, key: &str, content: &str) -> String {
    if !is_safe_key(key) {
        return bad_key(key);
    }
    if let Err(e) = fs::create_dir_all(workflow_dir) {
        return format!("Error creating workflow dir: {}", e);
    }
    let path = workflow_dir.join(format!("{}.md", key));
    if path.exists() {
        return format!("Error: Workflow '{}' already exists. Use workflow_delete first, then create it again.", key);
    }
    match parse_workflow(content) {
        None => "Error: Invalid workflow format. Must have at least one step like: - Label: `command`".to_string(),
        Some(wf) => {
            match fs::write(&path, content) {
                Ok(()) => format!("Workflow '{}' created with {} steps.", key, wf.steps.len()),
                Err(e) => format!("Error writing workflow: {}", e),
            }
        }
    }
}

/// Delete a workflow by key.
pub fn workflow_delete(workflow_dir: &Path, key: &str) -> String {
    if !is_safe_key(key) {
        return bad_key(key);
    }
    let path = workflow_dir.join(format!("{}.md", key));
    if !path.exists() {
        return format!("Error: Workflow '{}' not found.", key);
    }
    match fs::remove_file(&path) {
        Ok(())  => format!("Workflow '{}' deleted.", key),
        Err(e)  => format!("Error deleting workflow: {}", e),
    }
}
