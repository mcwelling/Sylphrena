//! Permission rules: allow, ask or deny a tool call.
//!
//! Rules come from `[[permission]]` tables in `.syl/config.toml`, then from
//! `.syl/approvals.toml` (written when the user answers "always"), then from
//! the mode (plan mode denies edits). They are checked in that order and the
//! **last** matching rule wins. With no matching rule a call is allowed: the
//! container limits the damage, and checkpoints undo it.
//!
//! ```toml
//! [[permission]]
//! tool = "run_bash"
//! pattern = "git push*"
//! action = "ask"
//! ```
//!
//! A rule matches a tool (`*` for any) and a glob over the call's subject:
//! the command for `run_bash`, the path for file tools, the URL for
//! `dispatch_agent`. A `run_bash` command is split into its parts
//! (`git status && rm -rf x` is two), each checked on its own, and the
//! strictest answer applies.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(default = "any")]
    pub tool: String,
    #[serde(default = "any")]
    pub pattern: String,
    pub action: Action,
}

fn any() -> String {
    "*".to_string()
}

/// How the agent works: `Build` may change files; `Plan` may not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Build,
    Plan,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Build => "build",
            Mode::Plan => "plan",
        }
    }
}

/// Tools that change files in the workspace (or Syl's workflows): denied
/// in plan mode.
pub const EDITING_TOOLS: &[&str] = &["edit_file", "write_file", "workflow_write", "workflow_delete"];

/// Commands plan mode refuses outright: they change the user's git
/// repository (which snapshots don't cover), delete ignored files, or
/// reach outside. Anything else `run_bash` changes in the workspace is put
/// back afterwards. Matched against normalised commands, so `git -C x
/// commit` and `sh -c 'git commit'` count.
const PLAN_DENIED_COMMANDS: &[&str] = &[
    "git commit*", "git push*", "git pull*", "git fetch*", "git merge*", "git rebase*", "git reset*",
    "git checkout*", "git switch*", "git restore*", "git stash*", "git cherry-pick*", "git revert*",
    "git am *", "git am", "git apply*", "git clean*", "git rm *", "git mv *", "git tag*", "git branch *-d*",
    "git branch *-D*", "git branch *-m*", "git branch *-M*", "git config*", "git update-ref*", "git gc*",
    "git notes*", "git worktree*", "git submodule*",
];

/// The decision for one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: Action,
    /// The subject that decided it (for `deny`, what was refused).
    pub subject: String,
    /// The rule that decided it, if any.
    pub rule: Option<Rule>,
    /// For `ask`: every subject that needs approval (a command like
    /// `git push a && git push b` has two), all shown and all approved.
    pub asks: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    rules: Vec<Rule>,
    /// "Always" answers: they turn an `ask` into an allow, and nothing else
    /// (a `deny` added to config.toml later still wins).
    approvals: Vec<Rule>,
}

impl Policy {
    pub fn new(rules: Vec<Rule>) -> Self {
        Policy { rules, approvals: Vec::new() }
    }

    pub fn with_approvals(mut self, approvals: Vec<Rule>) -> Policy {
        self.approvals = approvals;
        self
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// Also deny these tools outright (plan mode's MCP tools that don't say
    /// they only read). Added last, so they win.
    pub fn deny_tools(mut self, tools: &[String]) -> Policy {
        for t in tools {
            self.rules.push(Rule { tool: t.clone(), pattern: any(), action: Action::Deny });
        }
        self
    }

    /// These rules plus the ones `mode` adds, which come last and so win.
    pub fn with_mode(&self, mode: Mode) -> Policy {
        let mut rules = self.rules.clone();
        if mode == Mode::Plan {
            for tool in EDITING_TOOLS {
                rules.push(Rule { tool: tool.to_string(), pattern: any(), action: Action::Deny });
            }
            for pattern in PLAN_DENIED_COMMANDS {
                rules.push(Rule { tool: "run_bash".into(), pattern: pattern.to_string(), action: Action::Deny });
            }
        }
        Policy { rules, approvals: self.approvals.clone() }
    }

    fn action_for(&self, tool: &str, subject: &str) -> (Action, Option<Rule>) {
        let rule = self.rules.iter().rev().find(|r| glob_match(&r.tool, tool) && glob_match(&r.pattern, subject));
        let action = rule.map_or(Action::Allow, |r| r.action);
        if action == Action::Ask
            && self.approvals.iter().any(|a| a.action == Action::Allow && a.tool == tool && a.pattern == subject)
        {
            return (Action::Allow, rule.cloned());
        }
        (action, rule.cloned())
    }

    /// Decide a call. Each subject is decided by the last rule that matches
    /// it (allow when none does); the strictest subject decides the call.
    pub fn decide(&self, tool: &str, input: &serde_json::Value) -> Decision {
        let mut worst = Decision { action: Action::Allow, subject: String::new(), rule: None, asks: Vec::new() };
        for subject in subjects(tool, input) {
            let (action, rule) = self.action_for(tool, &subject);
            if action == Action::Ask && !worst.asks.contains(&subject) {
                worst.asks.push(subject.clone());
            }
            if action > worst.action || worst.subject.is_empty() {
                worst.action = action;
                worst.subject = subject;
                worst.rule = rule;
            }
        }
        worst
    }
}

/// A path as rules see it: relative to the workspace, without `./`,
/// repeated slashes or `..` steps, so one rule matches however the model
/// spells the path. Paths outside the workspace stay absolute.
pub fn normalize_path(path: &str) -> String {
    let p = path.trim();
    let (absolute, rest) = match p.strip_prefix(syl_container::WORKDIR) {
        Some(r) if r.is_empty() || r.starts_with('/') => (false, r),
        _ => (p.starts_with('/'), p),
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            // `..` above the start stays: the path leaves the workspace.
            ".." => {
                if parts.last().is_some_and(|p| *p != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            p => parts.push(p),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

/// What rules match against for a call.
pub fn subjects(tool: &str, input: &serde_json::Value) -> Vec<String> {
    let s = |k: &str| input[k].as_str().unwrap_or("").to_string();
    match tool {
        "run_bash" => {
            let mut parts = Vec::new();
            for part in split_commands(&s("command")) {
                for variant in command_variants(&part) {
                    if !parts.contains(&variant) {
                        parts.push(variant);
                    }
                }
            }
            if parts.is_empty() {
                vec![String::new()]
            } else {
                parts
            }
        }
        "read_file" | "edit_file" | "write_file" | "ls" | "show_image" => vec![normalize_path(&s("path"))],
        // A search reads files under `path`: rules on paths apply to it too.
        "grep" | "glob" => {
            let path = input["path"].as_str().map(normalize_path).unwrap_or_else(|| ".".to_string());
            vec![s("pattern"), path]
        }
        "dispatch_agent" => vec![s("source")],
        "memory_read" | "memory_write" => vec![s("key")],
        "workflow_write" | "workflow_delete" | "skill" => vec![s("name")],
        // MCP tools: their arguments, as compact JSON.
        _ => vec![if input.is_null() { String::new() } else { input.to_string() }],
    }
}

/// Split a shell command into the simple commands it runs: on `&&`, `||`,
/// `;`, `|`, `&` and newlines outside quotes, plus the commands inside
/// `$( )` and backticks. Leading `VAR=value` assignments are dropped, so
/// `FOO=1 git push` is checked as `git push`. This is a simple splitter,
/// not a shell parser: rules are a convenience, the container is the
/// boundary.
pub fn split_commands(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    // A backslash-newline continues the line.
    let cmd = cmd.replace("\\\n", " ");
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    let mut quote: Option<char> = None;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        let c = strip_prefixes(&collapse_spaces(cur));
        if !c.is_empty() {
            out.push(c);
        }
        cur.clear();
    };
    while i < chars.len() {
        let c = chars[i];
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some('\''), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                cur.push(c);
            }
            (_, '\\') if i + 1 < chars.len() => {
                cur.push(c);
                cur.push(chars[i + 1]);
                i += 1;
            }
            // Command substitution, also inside double quotes.
            (None | Some('"'), '$') if chars.get(i + 1) == Some(&'(') => {
                let (inner, end) = balanced(&chars, i + 2);
                out.extend(split_commands(&inner));
                cur.push_str("$(");
                cur.push_str(&inner);
                cur.push(')');
                i = end;
            }
            (None | Some('"'), '`') => {
                let end = chars[i + 1..].iter().position(|&c| c == '`').map_or(chars.len(), |p| i + 1 + p);
                let inner: String = chars[i + 1..end].iter().collect();
                out.extend(split_commands(&inner));
                cur.push('`');
                cur.push_str(&inner);
                cur.push('`');
                i = end;
            }
            // Process substitution `<( )` and `>( )` run commands too.
            (None, '(') if cur.ends_with('<') || cur.ends_with('>') => {
                let (inner, end) = balanced(&chars, i + 1);
                out.extend(split_commands(&inner));
                cur.push('(');
                cur.push_str(&inner);
                cur.push(')');
                i = end;
            }
            // Subshells and groups: `( git push )`, `{ git push; }`. A `{`
            // only counts at the start of a command, so `${VAR}` and brace
            // expansion stay words.
            (None, '(' | ')') => flush(&mut cur, &mut out),
            (None, '{' | '}') if cur.trim().is_empty() => flush(&mut cur, &mut out),
            // A comment runs to the end of the line.
            (None, '#') if cur.is_empty() || cur.ends_with(char::is_whitespace) => {
                while i + 1 < chars.len() && chars[i + 1] != '\n' {
                    i += 1;
                }
            }
            // `2>&1`, `>&2` and `&>file` are redirections, not separators.
            (None, '&') if cur.ends_with('>') || chars.get(i + 1) == Some(&'>') => cur.push(c),
            (None, ';' | '\n' | '|' | '&') => {
                flush(&mut cur, &mut out);
                // `&&` and `||` are one separator.
                if matches!(c, '|' | '&') && chars.get(i + 1) == Some(&c) {
                    i += 1;
                }
            }
            (_, c) => cur.push(c),
        }
        i += 1;
    }
    flush(&mut cur, &mut out);
    out
}

/// The text up to the `)` matching an already-consumed `(`, and the index
/// of that `)`. Parentheses inside quotes don't count.
fn balanced(chars: &[char], start: usize) -> (String, usize) {
    let mut depth = 1;
    let mut i = start;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        match (quote, chars[i]) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(chars[i]),
            (None, '\\') => i += 1,
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return (chars[start..i].iter().collect(), i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    (chars[start..].iter().collect(), chars.len())
}

fn collapse_spaces(cmd: &str) -> String {
    cmd.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first shell word of `cmd` (quotes honoured) and the rest after it.
fn first_word(cmd: &str) -> (String, &str) {
    let mut word = String::new();
    let mut quote: Option<char> = None;
    for (i, c) in cmd.char_indices() {
        match (quote, c) {
            (None, c) if c.is_whitespace() => return (word, cmd[i..].trim_start()),
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (_, c) => word.push(c),
        }
    }
    (word, "")
}

/// Drop what runs a command rather than being it: `VAR=value` assignments,
/// reserved words (`then`, `do`, `!`, `time` ...) and wrappers (`env`,
/// `sudo`, `nohup`, `timeout 60`, `xargs` ...) with their options.
fn strip_prefixes(cmd: &str) -> String {
    let mut rest = cmd.trim();
    loop {
        let (word, after) = first_word(rest);
        if after.is_empty() {
            return rest.to_string();
        }
        let is_assignment = word
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        let next = match word.as_str() {
            _ if is_assignment => after,
            "then" | "do" | "else" | "elif" | "if" | "while" | "until" | "!" | "time" | "command" | "builtin"
            | "exec" | "nohup" | "setsid" | "stdbuf" => after,
            "env" => skip_options(after, &["-u", "-C", "-S"]),
            "sudo" | "doas" => skip_options(after, &["-u", "-g", "-h", "-p", "-C", "-D", "-r", "-t", "-U"]),
            "xargs" => skip_options(after, &["-I", "-L", "-n", "-P", "-d", "-E", "-s", "-a"]),
            "nice" | "ionice" => skip_options(after, &["-n", "-c"]),
            "timeout" => {
                // Options, then the duration.
                let r = skip_options(after, &["-s", "-k"]);
                first_word(r).1
            }
            _ => return rest.to_string(),
        };
        rest = next.trim_start();
    }
}

/// Skip leading `-x` options, and the value after those in `with_value`
/// (`sudo -u me`, `nice -n 5`).
fn skip_options<'a>(mut rest: &'a str, with_value: &[&str]) -> &'a str {
    loop {
        let (word, after) = first_word(rest);
        if word.starts_with('-') && !after.is_empty() {
            rest = after;
            if with_value.contains(&word.as_str()) {
                rest = first_word(rest).1;
            }
        } else {
            return rest;
        }
    }
}

/// The forms of one simple command that rules are checked against: the
/// command as written, with its program's path dropped (`/usr/bin/git` is
/// `git`), with git's global options dropped (`git -C repo push` is `git
/// push`), and the commands inside `sh -c '...'` or `eval ...`.
pub fn command_variants(cmd: &str) -> Vec<String> {
    let mut out = vec![cmd.to_string()];
    let (prog, args) = first_word(cmd);
    let base = prog.rsplit('/').next().unwrap_or(&prog).to_string();
    let plain = if args.is_empty() { base.clone() } else { format!("{base} {args}") };
    if plain != cmd {
        out.push(plain.clone());
    }
    match base.as_str() {
        "git" => {
            let mut rest = args;
            loop {
                let (w, after) = first_word(rest);
                match w.as_str() {
                    "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => rest = first_word(after).1,
                    w if w.starts_with('-') && !after.is_empty() => rest = after,
                    _ => break,
                }
            }
            let g = format!("git {rest}").trim_end().to_string();
            if !out.contains(&g) {
                out.push(g);
            }
        }
        "sh" | "bash" | "zsh" | "dash" | "ksh" => {
            let mut rest = args;
            while !rest.is_empty() {
                let (w, after) = first_word(rest);
                if w.starts_with('-') && w.contains('c') {
                    let (script, _) = first_word(after);
                    for inner in split_commands(&script) {
                        out.extend(command_variants(&inner));
                    }
                    break;
                }
                if !w.starts_with('-') {
                    break;
                }
                rest = after;
            }
        }
        "eval" => {
            for inner in split_commands(args) {
                out.extend(command_variants(&inner));
            }
        }
        _ => {}
    }
    out
}

/// Glob match over the whole string: `*` is any run of characters
/// (slashes included), `?` is one character.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Rules saved by "always" answers, in `.syl/approvals.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Approvals {
    #[serde(default)]
    permission: Vec<Rule>,
}

/// The saved approvals; none when the file doesn't exist. A file that
/// doesn't parse is an error, so it is never silently replaced.
pub fn load_approvals(path: &std::path::Path) -> anyhow::Result<Vec<Rule>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
        Ok(t) => toml::from_str::<Approvals>(&t)
            .map(|a| a.permission)
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display())),
    }
}

/// Add an allow rule for exactly this subject to the approvals file.
pub fn save_approval(path: &std::path::Path, tool: &str, subject: &str) -> anyhow::Result<Rule> {
    let mut all = Approvals { permission: load_approvals(path)? };
    // Match the subject literally: escape nothing, but a subject with glob
    // characters would match more than it says, so those are refused.
    if subject.contains(['*', '?']) {
        anyhow::bail!("can't save an \"always\" rule for a subject containing * or ?");
    }
    let rule = Rule { tool: tool.to_string(), pattern: subject.to_string(), action: Action::Allow };
    if !all.permission.contains(&rule) {
        all.permission.push(rule.clone());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = format!(
        "# Written by Syl when you answer \"always\". Delete a rule to be asked again.\n{}",
        toml::to_string(&all)?
    );
    // Write a temp file and rename it over, so a turn reading the file at
    // that moment never sees it half-written.
    let tmp = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(rule)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(tool: &str, pattern: &str, action: Action) -> Rule {
        Rule { tool: tool.into(), pattern: pattern.into(), action }
    }

    #[test]
    fn splitting() {
        assert_eq!(split_commands("git status && rm -rf x"), ["git status", "rm -rf x"]);
        assert_eq!(split_commands("a; b | c || d & e\nf"), ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(split_commands("echo 'a && b' \"c; d\""), ["echo 'a && b' \"c; d\""]);
        assert_eq!(split_commands("FOO=1 BAR=x git push origin"), ["git push origin"]);
        let parts = split_commands("echo $(git push) `rm x`");
        assert!(parts.contains(&"git push".to_string()) && parts.contains(&"rm x".to_string()), "{parts:?}");
        assert_eq!(split_commands("ls 2>&1 | grep x"), ["ls 2>&1", "grep x"]);
        assert_eq!(split_commands("make &> log && echo ok"), ["make &> log", "echo ok"]);
    }

    #[test]
    fn natural_ways_to_write_a_command_are_all_checked() {
        let p = Policy::new(vec![rule("run_bash", "git push*", Action::Ask), rule("*", "rm -rf *", Action::Deny)]);
        let d = |cmd: &str| p.decide("run_bash", &json!({"command": cmd})).action;
        for cmd in [
            "cd repo && git push",
            "git -C repo push",
            "git -c core.x=1 push origin",
            "timeout 60 git push",
            "env FOO=1 git push",
            "sudo -u me git push",
            "nohup git push &",
            "command git push",
            "if true; then git push; fi",
            "for r in a b; do git push; done",
            "( git push )",
            "{ git push; }",
            "GIT_SSH_COMMAND=\"ssh -i k\" git push",
            "git  push",
            "git \\\npush",
            "bash -c 'git push'",
            "sh -lc \"git push origin\"",
            "eval git push",
            "/usr/bin/git push",
            "echo $(echo ')'; git push)",
            "diff <(git push) x",
        ] {
            assert_eq!(d(cmd), Action::Ask, "{cmd}");
        }
        assert_eq!(d("sh -c \"rm -rf x\""), Action::Deny);
        assert_eq!(d("echo '${HOME}' {a,b}"), Action::Allow);
        assert_eq!(d("ls # git push"), Action::Allow, "a comment runs nothing");
    }

    #[test]
    fn every_part_that_needs_asking_is_listed() {
        let p = Policy::new(vec![rule("run_bash", "git push*", Action::Ask)]);
        let d = p.decide("run_bash", &json!({"command": "git push origin main && git push --force evil main"}));
        assert_eq!(d.action, Action::Ask);
        assert!(d.asks.contains(&"git push origin main".into()) && d.asks.contains(&"git push --force evil main".into()), "{:?}", d.asks);
    }

    #[test]
    fn paths_are_normalised_and_searches_match_on_their_path() {
        let p = Policy::new(vec![rule("*", "src/*", Action::Ask), rule("*", ".env", Action::Deny)]);
        for path in ["src/a.rs", "./src/a.rs", "/workspace/src/a.rs", " src/a.rs", "lib/../src/a.rs", "src//a.rs"] {
            assert_eq!(p.decide("edit_file", &json!({"path": path})).action, Action::Ask, "{path}");
        }
        assert_eq!(p.decide("grep", &json!({"pattern": "KEY", "path": "./.env"})).action, Action::Deny);
        assert_eq!(normalize_path("/etc/../etc/passwd"), "/etc/passwd");
        assert_eq!(normalize_path("/workspace"), ".");
    }

    #[test]
    fn an_always_answer_never_overrides_a_deny() {
        let approvals = vec![rule("run_bash", "git push origin main", Action::Allow)];
        let asking = Policy::new(vec![rule("run_bash", "git push*", Action::Ask)]).with_approvals(approvals.clone());
        assert_eq!(asking.decide("run_bash", &json!({"command": "git push origin main"})).action, Action::Allow);
        let denying = Policy::new(vec![rule("run_bash", "git push*", Action::Deny)]).with_approvals(approvals);
        assert_eq!(denying.decide("run_bash", &json!({"command": "git push origin main"})).action, Action::Deny);
    }

    #[test]
    fn plan_mode_denies_git_commands_that_rewrite_the_repository() {
        let p = Policy::default().with_mode(Mode::Plan);
        for cmd in ["git reset --hard HEAD~3", "git -C . commit -m x", "sh -c 'git stash'", "git checkout main", "git clean -fdx"] {
            assert_eq!(p.decide("run_bash", &json!({"command": cmd})).action, Action::Deny, "{cmd}");
        }
        for cmd in ["git status", "git log --oneline", "git diff", "git branch", "git show HEAD"] {
            assert_eq!(p.decide("run_bash", &json!({"command": cmd})).action, Action::Allow, "{cmd}");
        }
    }

    #[test]
    fn a_corrupt_approvals_file_is_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("approvals.toml");
        std::fs::write(&path, "[[permission]]\ntool = \"x\"\naction = \"bogus\"\n").unwrap();
        assert!(load_approvals(&path).is_err());
        assert!(save_approval(&path, "run_bash", "ls").is_err());
        assert!(std::fs::read_to_string(&path).unwrap().contains("bogus"));
    }

    #[test]
    fn globs() {
        assert!(glob_match("git push*", "git push origin main"));
        assert!(glob_match("*", ""));
        assert!(glob_match("src/*.rs", "src/a/b.rs"));
        assert!(!glob_match("git push*", "git status"));
        assert!(glob_match("r?n", "run"));
    }

    #[test]
    fn last_match_wins_and_the_strictest_part_decides() {
        let p = Policy::new(vec![
            rule("run_bash", "*", Action::Ask),
            rule("run_bash", "git status*", Action::Allow),
            rule("run_bash", "rm -rf *", Action::Deny),
        ]);
        let d = |cmd: &str| p.decide("run_bash", &json!({"command": cmd})).action;
        assert_eq!(d("git status"), Action::Allow);
        assert_eq!(d("ls"), Action::Ask);
        assert_eq!(d("git status && rm -rf x"), Action::Deny);
        let decision = p.decide("run_bash", &json!({"command": "git status && rm -rf x"}));
        assert_eq!(decision.subject, "rm -rf x");
        assert_eq!(Policy::default().decide("write_file", &json!({"path": "a"})).action, Action::Allow);
    }

    #[test]
    fn plan_mode_denies_edits_whatever_the_rules_say() {
        let p = Policy::new(vec![rule("*", "*", Action::Allow)]).with_mode(Mode::Plan);
        assert_eq!(p.decide("edit_file", &json!({"path": "a"})).action, Action::Deny);
        assert_eq!(p.decide("read_file", &json!({"path": "a"})).action, Action::Allow);
        assert_eq!(p.decide("run_bash", &json!({"command": "ls && git push"})).action, Action::Deny);
        assert_eq!(p.decide("run_bash", &json!({"command": "cargo test"})).action, Action::Allow);
    }

    #[test]
    fn always_answers_are_saved_and_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("approvals.toml");
        save_approval(&path, "run_bash", "git push origin main").unwrap();
        save_approval(&path, "run_bash", "git push origin main").unwrap();
        let rules = load_approvals(&path).unwrap();
        assert_eq!(rules, [rule("run_bash", "git push origin main", Action::Allow)]);
        assert!(save_approval(&path, "run_bash", "rm *").is_err());
    }
}
