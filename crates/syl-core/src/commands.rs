//! Custom slash commands and Agent Skills: plain markdown files.
//!
//! **Commands** are prompt templates: `.syl/commands/<name>.md` in the
//! project, or `~/.syl/commands/<name>.md` for every project (the project's
//! wins). `/name some words` sends the file's text as the prompt, with
//! `$ARGUMENTS` (or `{{args}}`) replaced by `some words`; without either,
//! the words are added at the end. Optional front matter:
//!
//! ```markdown
//! ---
//! description: Review the staged changes
//! ---
//! Review `git diff --staged` for bugs. Focus on: $ARGUMENTS
//! ```
//!
//! **Skills** follow the Agent Skills format: a directory with a `SKILL.md`
//! whose front matter has `name` and `description`. They are found in
//! `.syl/skills/`, `.agents/skills/` (both in the project) and
//! `~/.syl/skills/`. Only names and descriptions go in the system prompt;
//! the model loads a skill's instructions with the `skill` tool when a task
//! calls for it, and `/skill:name words` loads one directly.
use std::path::{Path, PathBuf};

/// A slash command or a skill.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub name: String,
    pub description: String,
    /// The markdown after the front matter.
    pub body: String,
    /// Where it came from.
    pub path: PathBuf,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Split `---` front matter from the body. Fields are `key: value` lines;
/// a value of `>` or `|` takes the indented lines after it (folded into one
/// line, or kept as lines). Without a closing `---` line it's all body.
pub fn front_matter(text: &str) -> (Vec<(String, String)>, String) {
    let t = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = t.split_inclusive('\n');
    if lines.next().map(|l| l.trim_end_matches(['\n', '\r'])) != Some("---") {
        return (Vec::new(), t.to_string());
    }
    let mut header: Vec<&str> = Vec::new();
    let mut offset = t.split_inclusive('\n').next().map_or(0, str::len);
    let mut closed = false;
    for l in lines {
        offset += l.len();
        let line = l.trim_end_matches(['\n', '\r']);
        if line == "---" {
            closed = true;
            break;
        }
        header.push(line);
    }
    if !closed {
        return (Vec::new(), t.to_string());
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < header.len() {
        let line = header[i];
        i += 1;
        if line.starts_with([' ', '\t']) || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        let value = if let Some(style) = v.strip_prefix(['>', '|']).map(|_| v.starts_with('|')) {
            let mut block = Vec::new();
            while i < header.len() && (header[i].starts_with([' ', '\t']) || header[i].trim().is_empty()) {
                block.push(header[i].trim());
                i += 1;
            }
            if style { block.join("\n").trim().to_string() } else { block.join(" ").split_whitespace().collect::<Vec<_>>().join(" ") }
        } else {
            v.trim_matches(|c| c == '"' || c == '\'').to_string()
        };
        fields.push((k.trim().to_string(), value));
    }
    (fields, t[offset..].to_string())
}

fn field(fields: &[(String, String)], key: &str) -> Option<String> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()).filter(|v| !v.is_empty())
}

/// The first line of the body, as a fallback description.
fn first_line(body: &str) -> String {
    let line = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    crate::session::one_line(line.trim_start_matches('#').trim(), 100)
}

fn command_dirs(project: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project.join(".syl/commands")];
    dirs.extend(home().map(|h| h.join(".syl/commands")));
    dirs
}

fn skill_dirs(project: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![project.join(".syl/skills"), project.join(".agents/skills")];
    dirs.extend(home().map(|h| h.join(".syl/skills")));
    dirs
}

/// Every custom command, the project's first; a name already seen is
/// skipped. Symbolic links are not followed: a cloned project could ship
/// `review.md -> ~/.ssh/id_rsa`.
pub fn list_commands(project: &Path) -> Vec<Template> {
    let mut out: Vec<Template> = Vec::new();
    let home = home();
    for dir in command_dirs(project) {
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
            if !crate::util::is_safe_key(&name) || out.iter().any(|t| t.name == name) {
                continue;
            }
            let Ok(text) = crate::util::read_regular(&path) else { continue };
            let (fields, body) = front_matter(&text);
            let description = field(&fields, "description").unwrap_or_else(|| first_line(&body));
            out.push(Template { name, description, body, path });
        }
    }
    out
}

fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(true)
}

/// A skill directory root that is really inside `base` (the project or
/// home): code in the container can write the project's `.agents/`, and a
/// symbolic link there would point Syl, reading on the host, anywhere.
fn contained(dir: &Path, base: &Path) -> bool {
    match (dir.canonicalize(), base.canonicalize()) {
        (Ok(d), Ok(b)) => d.starts_with(b),
        _ => false,
    }
}

/// Every skill, the project's first; a name already seen is skipped.
/// Symbolic links are not followed.
pub fn list_skills(project: &Path) -> Vec<Template> {
    let mut out: Vec<Template> = Vec::new();
    let home = home();
    for dir in skill_dirs(project) {
        let base = if dir.starts_with(project) { project.to_path_buf() } else { home.clone().unwrap_or_default() };
        if !contained(&dir, &base) {
            continue;
        }
        let Ok(read) = std::fs::read_dir(&dir) else { continue };
        let mut paths: Vec<PathBuf> = read.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for skill_dir in paths {
            let file = skill_dir.join("SKILL.md");
            if is_link(&skill_dir) || is_link(&file) {
                continue;
            }
            let Ok(text) = crate::util::read_regular(&file) else { continue };
            let (fields, body) = front_matter(&text);
            let dir_name = skill_dir.file_name().and_then(|s| s.to_str()).unwrap_or("").to_string();
            let name = field(&fields, "name").unwrap_or(dir_name);
            if !crate::util::is_safe_key(&name) || out.iter().any(|t| t.name == name) {
                continue;
            }
            let description = field(&fields, "description").unwrap_or_else(|| first_line(&body));
            out.push(Template { name, description, body, path: file });
        }
    }
    out
}

/// The system prompt section listing skills, or None when there are none.
pub fn skills_prompt(skills: &[Template]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut s = String::from(
        "\n### SKILLS\nThese skills hold instructions for particular tasks. When a task matches one, load it \
         with the `skill` tool (by name) before starting, and follow it.\n",
    );
    for t in skills {
        s.push_str(&format!("- {}: {}\n", t.name, crate::session::one_line(&t.description, 300)));
    }
    Some(s)
}

/// The `skill` tool: a skill's instructions, and the other files beside it.
pub fn load_skill(project: &Path, name: &str) -> (String, bool) {
    let skills = list_skills(project);
    let Some(skill) = skills.iter().find(|s| s.name == name) else {
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        return (format!("[Tool error: no skill named {name:?}. Available: {}]", names.join(", ")), true);
    };
    let dir = skill.path.parent().unwrap_or(project);
    let mut files: Vec<String> = Vec::new();
    collect_files(dir, dir, &mut files, 0);
    files.retain(|f| f != "SKILL.md");
    // Files under the project are visible in the container at /workspace.
    let where_ = match dir.strip_prefix(project) {
        Ok(rel) => format!("/workspace/{}", rel.display()),
        Err(_) => "outside the workspace (not readable from the container)".to_string(),
    };
    let mut out = format!("# Skill: {}\n\n{}", skill.name, skill.body.trim());
    if !files.is_empty() {
        out.push_str(&format!("\n\n---\nOther files in this skill ({where_}):\n"));
        for f in files.iter().take(50) {
            out.push_str(&format!("- {f}\n"));
        }
    }
    (out, false)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 3 {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = read.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if is_link(&p) {
            continue;
        }
        if p.is_dir() {
            collect_files(root, &p, out, depth + 1);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

/// A prompt that starts with `/name` or `/skill:name`, expanded. None when
/// it names no command or skill (it is sent as written).
pub fn expand(project: &Path, prompt: &str) -> Option<String> {
    let rest = prompt.trim_start().strip_prefix('/')?;
    let (word, args) = match rest.split_once(char::is_whitespace) {
        Some((w, a)) => (w, a.trim()),
        None => (rest.trim(), ""),
    };
    if let Some(skill) = word.strip_prefix("skill:") {
        let (body, is_error) = load_skill(project, skill);
        if is_error {
            return None;
        }
        return Some(if args.is_empty() {
            format!("{body}\n\n---\nFollow this skill.")
        } else {
            format!("{body}\n\n---\nFollow this skill for: {args}")
        });
    }
    let command = list_commands(project).into_iter().find(|c| c.name == word)?;
    let body = command.body.trim();
    Some(if body.contains("$ARGUMENTS") || body.contains("{{args}}") {
        substitute(body, args)
    } else if args.is_empty() {
        body.to_string()
    } else {
        format!("{body}\n\n{args}")
    })
}

/// `$ARGUMENTS` and `{{args}}` replaced in one pass, so the arguments
/// themselves are never substituted into.
fn substitute(body: &str, args: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let next = ["$ARGUMENTS", "{{args}}"].iter().filter_map(|m| rest.find(m).map(|i| (i, m.len()))).min();
        let Some((i, len)) = next else { break };
        out.push_str(&rest[..i]);
        out.push_str(args);
        rest = &rest[i + len..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_is_split_off() {
        let (f, body) = front_matter("---\nname: review\ndescription: \"Check things\"\n---\nDo it.\n");
        assert_eq!(field(&f, "name").as_deref(), Some("review"));
        assert_eq!(field(&f, "description").as_deref(), Some("Check things"));
        assert_eq!(body, "Do it.\n");
        assert_eq!(front_matter("no matter").1, "no matter");
        assert_eq!(front_matter("---\n---\nBody").1, "Body");
        assert_eq!(front_matter("---\nBody text\n--- not fm").1, "---\nBody text\n--- not fm");
        let (f, body) = front_matter("---\nname: x\ndescription: >\n  Long text\n  over lines\nnotes: |\n  a\n  b\n---\nB");
        assert_eq!(field(&f, "description").as_deref(), Some("Long text over lines"));
        assert_eq!(field(&f, "notes").as_deref(), Some("a\nb"));
        assert_eq!(body, "B");
    }

    #[test]
    fn arguments_are_substituted_once() {
        assert_eq!(substitute("A $ARGUMENTS B {{args}}", "x {{args}} $ARGUMENTS"), "A x {{args}} $ARGUMENTS B x {{args}} $ARGUMENTS");
    }

    #[cfg(unix)]
    #[test]
    fn commands_do_not_follow_symbolic_links() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("id_rsa"), "PRIVATE KEY").unwrap();
        let dir = tmp.path().join(".syl/commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(outside.path().join("id_rsa"), dir.join("review.md")).unwrap();
        std::fs::write(dir.join("ok.md"), "fine").unwrap();
        assert_eq!(list_commands(tmp.path()).into_iter().map(|c| c.name).collect::<Vec<_>>(), ["ok"]);
        assert!(expand(tmp.path(), "/review").is_none());
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp2.path().join(".syl")).unwrap();
        std::fs::create_dir_all(outside.path().join("cmds")).unwrap();
        std::fs::write(outside.path().join("cmds/x.md"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("cmds"), tmp2.path().join(".syl/commands")).unwrap();
        assert!(list_commands(tmp2.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn skills_do_not_follow_symbolic_links() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "TOKEN=hunter2").unwrap();
        std::fs::create_dir_all(outside.path().join("skill")).unwrap();
        std::fs::write(outside.path().join("skill/SKILL.md"), "---\nname: out\n---\nTOKEN").unwrap();
        let skills = tmp.path().join(".agents/skills");
        std::fs::create_dir_all(skills.join("leak")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), skills.join("leak/SKILL.md")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("skill"), skills.join("linked")).unwrap();
        std::fs::create_dir_all(skills.join("ok")).unwrap();
        std::fs::write(skills.join("ok/SKILL.md"), "---\nname: ok\ndescription: fine\n---\nx").unwrap();
        std::os::unix::fs::symlink(outside.path(), skills.join("ok/host")).unwrap();
        let names: Vec<String> = list_skills(tmp.path()).into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["ok"]);
        assert!(!load_skill(tmp.path(), "ok").0.contains("secret"));
        // The whole directory linked out of the project.
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp2.path().join(".agents")).unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp2.path().join(".agents/skills")).unwrap();
        assert!(list_skills(tmp2.path()).is_empty());
    }

    #[test]
    fn commands_expand_with_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".syl/commands");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("review.md"), "---\ndescription: Review\n---\nReview the diff. Focus: $ARGUMENTS").unwrap();
        std::fs::write(dir.join("plain.md"), "Summarise the README.").unwrap();
        assert_eq!(expand(tmp.path(), "/review error handling").unwrap(), "Review the diff. Focus: error handling");
        assert_eq!(expand(tmp.path(), "/plain").unwrap(), "Summarise the README.");
        assert_eq!(expand(tmp.path(), "/plain briefly").unwrap(), "Summarise the README.\n\nbriefly");
        assert!(expand(tmp.path(), "/nope").is_none());
        assert!(expand(tmp.path(), "not a command").is_none());
        let cmds = list_commands(tmp.path());
        assert!(cmds.iter().any(|c| c.name == "review" && c.description == "Review"));
    }

    #[test]
    fn skills_are_listed_and_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".agents/skills/pdf");
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: pdf\ndescription: Work with PDF files\n---\n# PDF\nUse pdftotext.").unwrap();
        std::fs::write(dir.join("scripts/split.py"), "").unwrap();
        let skills = list_skills(tmp.path());
        assert_eq!(skills.len(), 1);
        assert!(skills_prompt(&skills).unwrap().contains("- pdf: Work with PDF files"));
        let (body, is_error) = load_skill(tmp.path(), "pdf");
        assert!(!is_error);
        assert!(body.contains("Use pdftotext") && body.contains("scripts/split.py") && body.contains("/workspace/.agents/skills/pdf"));
        assert!(load_skill(tmp.path(), "nope").1);
        assert!(expand(tmp.path(), "/skill:pdf split a.pdf").unwrap().contains("Follow this skill for: split a.pdf"));
    }
}
