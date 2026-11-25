use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::util::is_safe_key;

fn bad_key(key: &str) -> String {
    format!("Error: invalid workflow name '{}'. Use letters, digits, '-', '_' or '.' (no '..', no leading '.').", key)
}

/// `- Label: `command`` with an optional `(retry N)` after it.
static STEP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*[-*]\s+(.+?):\s*`(.+?)`\s*(?:\(retry\s+(\d+)\))?\s*$").unwrap());
/// `- Label: ask: a prompt for Syl`.
static ASK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*[-*]\s+([^`]+?):\s*ask:\s*(.+?)\s*$").unwrap());
static PARAM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{\s*([A-Za-z0-9_-]+)\s*\}\}").unwrap());

/// What a step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Run `command` in the container.
    Run,
    /// Send `command` (the text) to Syl as a prompt; the step is done when
    /// her turn ends.
    Ask,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowStep {
    pub label:   String,
    /// The command, or for an `ask` step the prompt.
    pub command: String,
    pub kind:    StepKind,
    /// Extra attempts for a command that fails (`(retry N)`).
    pub retries: u32,
}

/// A workflow (a recipe): markdown steps, optionally with parameters.
///
/// ```markdown
/// ---
/// params: version
/// ---
/// # Release
/// Cut a release.
///
/// - Test: `cargo test` (retry 2)
/// - Bump: `sed -i 's/^version = .*/version = "{{version}}"/' Cargo.toml`
/// - Changelog: ask: Update CHANGELOG.md for {{version}} from the git log.
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    pub name:        String,
    pub description: Option<String>,
    pub steps:       Vec<WorkflowStep>,
    /// Parameter names from the front matter, filled in as `{{name}}`.
    pub params:      Vec<String>,
}

impl Workflow {
    /// The workflow with its parameters filled in, from the words after
    /// its name (see [`split_args`]). Values come as `name=value` for a
    /// declared name, or in order before any named one. Every declared
    /// parameter needs a value.
    pub fn fill(&self, args: &[String]) -> Result<Workflow, String> {
        let mut values: std::collections::HashMap<String, String> = Default::default();
        let mut positional = self.params.iter();
        let mut named = false;
        for a in args {
            match a.split_once('=') {
                Some((k, v)) if self.params.iter().any(|p| p == k) => {
                    named = true;
                    values.insert(k.to_string(), v.to_string());
                }
                _ if named => {
                    return Err(format!(
                        "`{a}` comes after a named value, so it's unclear which parameter it is for; quote a value \
                         with spaces: name=\"two words\""
                    ))
                }
                _ => match positional.find(|p| !values.contains_key(*p)) {
                    Some(p) => {
                        values.insert(p.clone(), a.to_string());
                    }
                    None => return Err(format!("too many values for {}", self.name)),
                },
            }
        }
        let missing: Vec<&str> = self.params.iter().filter(|p| !values.contains_key(*p)).map(String::as_str).collect();
        if !missing.is_empty() {
            return Err(format!("{} needs: {} (give it as {}=..., or in order)", self.name, missing.join(", "), missing[0]));
        }
        let fill = |t: &str| PARAM_RE.replace_all(t, |c: &regex::Captures| values.get(&c[1]).cloned().unwrap_or_else(|| c[0].to_string())).into_owned();
        let mut out = self.clone();
        for step in &mut out.steps {
            step.command = fill(&step.command);
            step.label = fill(&step.label);
        }
        Ok(out)
    }
}

/// Words as a shell would split them: whitespace separates, and '...' or
/// "..." keep spaces (a backslash escapes the next character outside
/// single quotes).
pub fn split_args(s: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("a ' quote is not closed".to_string()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => word.extend(chars.next()),
                        Some(c) => word.push(c),
                        None => return Err("a \" quote is not closed".to_string()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.extend(chars.next());
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        out.push(word);
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct WorkflowEntry {
    pub key:  String,
    pub file: String,
}

/// Parse a workflow markdown string.
/// Returns None if there are no steps.
pub fn parse_workflow(content: &str) -> Option<Workflow> {
    let (fields, body) = crate::commands::front_matter(content);
    let params: Vec<String> = fields
        .iter()
        .find(|(k, _)| k == "params")
        .map(|(_, v)| {
            v.trim_matches(['[', ']'])
                .split(',')
                .map(|p| p.trim().trim_matches(['"', '\'']).to_string())
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let step_re = &*STEP_RE;
    let lines: Vec<&str> = body.lines().collect();

    let name = lines.iter()
        .filter(|l| l.trim().starts_with("# "))
        .map(|l| l.trim().trim_start_matches('#').trim().to_string())
        .next();

    let steps: Vec<WorkflowStep> = lines.iter()
        .filter_map(|l| {
            // A run step first: its command may contain ": ask:".
            if let Some(c) = step_re.captures(l) {
                return Some(WorkflowStep {
                    label:   c[1].trim().to_string(),
                    command: c[2].trim().to_string(),
                    kind:    StepKind::Run,
                    retries: c.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(0).min(10),
                });
            }
            ASK_RE.captures(l).map(|c| WorkflowStep {
                label:   c[1].trim().to_string(),
                command: c[2].trim().to_string(),
                kind:    StepKind::Ask,
                retries: 0,
            })
        })
        .collect();

    if steps.is_empty() {
        return None;
    }

    // Description: non-blank, non-heading, non-step lines before the first step
    let first_step_line = lines.iter().position(|l| step_re.is_match(l) || ASK_RE.is_match(l)).unwrap_or(lines.len());
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
        params,
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
    let content = crate::util::read_regular(&path).ok()?;
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
    if path.exists() || std::fs::symlink_metadata(&path).is_ok() {
        return format!("Error: Workflow '{}' already exists. Use workflow_delete first, then create it again.", key);
    }
    match parse_workflow(content) {
        None => "Error: Invalid workflow format. Must have at least one step like: - Label: `command`".to_string(),
        Some(wf) => {
            match crate::util::write_regular(&path, content) {
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

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPE: &str = "---\nparams: version, branch\n---\n# Release\nCut a release.\n\n\
- Test: `cargo test` (retry 2)\n\
- Bump: `echo {{version}} > VERSION`\n\
- Changelog: ask: Update CHANGELOG.md for {{version}} on {{branch}}.\n";

    #[test]
    fn recipes_parse_with_params_retries_and_prompts() {
        let wf = parse_workflow(RECIPE).unwrap();
        assert_eq!(wf.params, ["version", "branch"]);
        assert_eq!(wf.description.as_deref(), Some("Cut a release."));
        assert_eq!((wf.steps[0].kind, wf.steps[0].retries), (StepKind::Run, 2));
        assert_eq!(wf.steps[2].kind, StepKind::Ask);
        let args = |s: &str| split_args(s).unwrap();
        let filled = wf.fill(&args("1.2.0 branch=main")).unwrap();
        assert_eq!(filled.steps[1].command, "echo 1.2.0 > VERSION");
        assert_eq!(filled.steps[2].command, "Update CHANGELOG.md for 1.2.0 on main.");
        assert!(wf.fill(&args("1.2.0")).unwrap_err().contains("branch"));
        assert!(wf.fill(&args("x y z")).is_err());
        // A value with spaces must be quoted, not spread over parameters.
        assert!(wf.fill(&args("branch=fix bug")).unwrap_err().contains("quote"));
        assert_eq!(wf.fill(&args("1.2 branch='fix bug'")).unwrap().steps[2].command, "Update CHANGELOG.md for 1.2 on fix bug.");
        // `=` in a positional value is fine when it isn't a parameter name.
        assert_eq!(wf.fill(&args("https://x?a=b main")).unwrap().steps[1].command, "echo https://x?a=b > VERSION");
    }

    #[test]
    fn a_run_step_containing_ask_stays_a_run_step() {
        let wf = parse_workflow("# T\n- Grep: `grep -n 'TODO: ask: later' f`\n").unwrap();
        assert_eq!((wf.steps[0].kind, wf.steps[0].command.as_str()), (StepKind::Run, "grep -n 'TODO: ask: later' f"));
    }

    #[test]
    fn arguments_split_like_a_shell() {
        assert_eq!(split_args(r#"a "b c" 'd e' f\ g"#).unwrap(), ["a", "b c", "d e", "f g"]);
        assert!(split_args("'open").is_err());
    }

    #[test]
    fn plain_workflows_still_parse() {
        let wf = parse_workflow("# Ship\n- Test: `bb test`\n- Push: `git push`\n").unwrap();
        assert!(wf.params.is_empty());
        assert_eq!(wf.steps.iter().map(|s| s.kind).collect::<Vec<_>>(), [StepKind::Run, StepKind::Run]);
        assert_eq!(wf.fill(&[]).unwrap().steps[0].command, "bb test");
    }
}
