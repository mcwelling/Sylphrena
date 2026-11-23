//! File tools: `read_file`, `edit_file`, `write_file`, `grep`, `glob`, `ls`.
//!
//! All of them run inside the container, so the sandbox stays the boundary:
//! a path (or a symlink planted by code in the container) resolves in the
//! container's filesystem, never the host's. Content goes in over stdin and
//! comes back as bytes, so nothing is limited by argument length or quoting.
use std::time::Duration;

use syl_container::{Container, ExecError, RawOutput};
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::util::{truncate_output, MAX_TOOL_OUTPUT};

/// The most lines `grep`, `glob` and `ls` return before cutting off.
const MAX_LISTING_LINES: usize = 300;

type ToolResult = (String, bool);

fn err(msg: impl std::fmt::Display) -> ToolResult {
    (format!("[Tool error: {msg}]"), true)
}

fn str_arg<'a>(input: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    input[key].as_str()
}

fn timeout(config: &AppConfig) -> Duration {
    Duration::from_millis(config.timeout_ms)
}

async fn run(
    container: &Container,
    config: &AppConfig,
    argv: &[&str],
    stdin: Option<&[u8]>,
    cancel: &CancellationToken,
) -> Result<RawOutput, String> {
    container
        .exec_argv(argv, stdin, timeout(config), cancel)
        .await
        .map_err(|e| match e {
            ExecError::TimedOut(_) => format!("timed out after {}s", config.timeout_ms / 1000),
            other => other.to_string(),
        })
}

/// What is at a path.
enum Existing {
    Missing,
    Text(String),
    /// Not UTF-8: can be replaced, not edited or diffed.
    Binary,
}

/// A file's contents. Other failures (a directory, no permission) are errors.
async fn read_text(
    container: &Container,
    config: &AppConfig,
    path: &str,
    cancel: &CancellationToken,
) -> Result<Existing, String> {
    // Exit 3 means "not there", so a missing file isn't mistaken for an
    // unreadable one.
    let script = r#"if [ ! -e "$1" ]; then exit 3; fi; if [ -d "$1" ]; then echo "is a directory" >&2; exit 4; fi; cat -- "$1""#;
    let out = run(container, config, &["/bin/sh", "-c", script, "sh", path], None, cancel).await?;
    match out.code {
        Some(0) => Ok(String::from_utf8(out.stdout).map_or(Existing::Binary, Existing::Text)),
        Some(3) => Ok(Existing::Missing),
        _ => Err(format!("could not read {path}: {}", out.stderr.trim())),
    }
}

async fn write_text(
    container: &Container,
    config: &AppConfig,
    path: &str,
    content: &str,
    cancel: &CancellationToken,
) -> Result<(), String> {
    // Write a temp file beside the target, check every byte arrived, then
    // rename it over the target: a cancel, timeout or full disk part way
    // through leaves the old file whole. A symlink is written through to its
    // target, and an existing file's mode is kept.
    let script = r#"f="$1"; n="$2"
if [ -L "$f" ]; then f=$(readlink -f -- "$f") || exit 1; fi
d=$(dirname -- "$f"); mkdir -p -- "$d" || exit 1
tmp=$(mktemp "$d/.syl-write.XXXXXX") || exit 1
trap 'rm -f -- "$tmp"' EXIT
cat > "$tmp" || exit 1
[ "$(wc -c < "$tmp" | tr -d ' ')" = "$n" ] || { echo "incomplete write" >&2; exit 1; }
if [ -e "$f" ]; then chmod --reference="$f" "$tmp" 2>/dev/null || chmod "$(stat -f %Lp "$f" 2>/dev/null)" "$tmp" 2>/dev/null; else chmod 644 "$tmp"; fi
mv -f -- "$tmp" "$f" && trap - EXIT"#;
    let len = content.len().to_string();
    let out = run(container, config, &["/bin/sh", "-c", script, "sh", path, &len], Some(content.as_bytes()), cancel).await?;
    if out.success() {
        Ok(())
    } else {
        Err(format!("could not write {path}: {}", out.stderr.trim()))
    }
}

/// Above this many bytes on either side, a diff isn't worth computing.
const MAX_DIFF_INPUT: usize = 2_000_000;

/// A unified diff of `old` to `new`, cut to fit a tool result. Diffing can
/// take seconds on big rewrites, so it runs off the async runtime with a
/// deadline (past it, the diff is coarser but still correct).
pub async fn unified_diff_async(path: &str, old: &str, new: &str) -> String {
    if old.len() > MAX_DIFF_INPUT || new.len() > MAX_DIFF_INPUT {
        return format!("(diff skipped: {} -> {} bytes)", old.len(), new.len());
    }
    let (p, o, n) = (path.to_string(), old.to_string(), new.to_string());
    tokio::task::spawn_blocking(move || unified_diff(&p, &o, &n))
        .await
        .unwrap_or_else(|e| format!("(diff failed: {e})"))
}

/// A unified diff of `old` to `new`, cut to fit a tool result.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_millis(500))
        .diff_lines(old, new);
    let text = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    truncate_output(&text)
}

fn count_lines(s: &str) -> usize {
    s.lines().count()
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

/// Print lines `start_line..=end_line` (default: 100 lines) with numbers.
pub async fn read_file(
    input: &serde_json::Value,
    config: &AppConfig,
    container: &Container,
    cancel: &CancellationToken,
) -> ToolResult {
    let path = match str_arg(input, "path") {
        Some(p) if !p.trim().is_empty() => p,
        _ => return err("read_file requires a non-empty 'path' field"),
    };
    let start: u64 = input["start_line"].as_u64().unwrap_or(1).max(1);
    let end: u64 = input["end_line"].as_u64().unwrap_or(start + 99).max(start);
    let abs = container.resolve(path);
    let script = r#"f="$1"; s="$2"; e="$3"
if [ -d "$f" ]; then echo "Error: $f is a directory (use ls)"; exit 1; fi
if [ ! -f "$f" ]; then echo "Error: file not found: $f"; exit 1; fi
total=$(awk 'END {print NR}' "$f")
if [ "$total" -lt "$s" ]; then echo "[File: $f — $total lines total; line $s is past the end]"; exit 0; fi
[ "$e" -gt "$total" ] && e=$total
echo "[File: $f — $total lines total, showing $s-$e]"
awk -v s="$s" -v e="$e" 'NR>=s && NR<=e {printf "%5d: %s\n", NR, $0} NR>e {exit}' "$f""#;
    let (s, e) = (start.to_string(), end.to_string());
    match run(container, config, &["/bin/sh", "-c", script, "sh", &abs, &s, &e], None, cancel).await {
        Ok(out) if out.success() => (truncate_output(&String::from_utf8_lossy(&out.stdout)), false),
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let text = if text.is_empty() { out.stderr.trim().to_string() } else { text };
            (text, true)
        }
        Err(e) => err(e),
    }
}

// ---------------------------------------------------------------------------
// edit_file / write_file
// ---------------------------------------------------------------------------

/// Replace `old_string` with `new_string` in a file. The old string must
/// occur exactly once, unless `replace_all` is set. Returns a unified diff.
pub async fn edit_file(
    input: &serde_json::Value,
    config: &AppConfig,
    container: &Container,
    cancel: &CancellationToken,
) -> ToolResult {
    let (Some(path), Some(old), Some(new)) =
        (str_arg(input, "path"), str_arg(input, "old_string"), str_arg(input, "new_string"))
    else {
        return err("edit_file requires 'path', 'old_string' and 'new_string'");
    };
    if old.is_empty() {
        return err("old_string is empty; use write_file to create or replace a whole file");
    }
    if old == new {
        return err("old_string and new_string are the same; nothing to change");
    }
    let replace_all = input["replace_all"].as_bool().unwrap_or(false);
    let abs = container.resolve(path);
    let current = match read_text(container, config, &abs, cancel).await {
        Ok(Existing::Text(t)) => t,
        Ok(Existing::Missing) => return err(format!("file not found: {abs} (use write_file to create it)")),
        Ok(Existing::Binary) => return err(format!("{abs} is not UTF-8 text; use write_file to replace it")),
        Err(e) => return err(e),
    };
    // read_file doesn't show "\r", so on a CRLF file the model writes "\n"
    // line breaks: match (and write) them as CRLF.
    let crlf_old;
    let crlf_new;
    let (old, new) = if current.contains("\r\n") && old.contains('\n') && !old.contains('\r') && !current.contains(old) {
        crlf_old = old.replace('\n', "\r\n");
        crlf_new = new.replace("\r\n", "\n").replace('\n', "\r\n");
        (crlf_old.as_str(), crlf_new.as_str())
    } else {
        (old, new)
    };
    let count = current.matches(old).count();
    let updated = match (count, replace_all) {
        (0, _) => {
            let hint = if current.contains(old.trim()) {
                " (it does appear with different surrounding whitespace; copy it exactly, including indentation)"
            } else {
                " (read the file again to get its current text)"
            };
            return err(format!("old_string was not found in {abs}{hint}"));
        }
        (1, _) | (_, true) => current.replace(old, new),
        (n, false) => {
            return err(format!(
                "old_string occurs {n} times in {abs}; include more surrounding text to make it unique, \
                 or set replace_all to change every occurrence"
            ))
        }
    };
    if let Err(e) = write_text(container, config, &abs, &updated, cancel).await {
        return err(e);
    }
    let what = if count == 1 { "1 replacement".to_string() } else { format!("{count} replacements") };
    (format!("Edited {abs} ({what}).\n{}", unified_diff_async(path, &current, &updated).await), false)
}

/// Create a file, or replace its contents. Returns a diff against what was
/// there.
pub async fn write_file(
    input: &serde_json::Value,
    config: &AppConfig,
    container: &Container,
    cancel: &CancellationToken,
) -> ToolResult {
    let (Some(path), Some(content)) = (str_arg(input, "path"), str_arg(input, "content")) else {
        return err("write_file requires 'path' and 'content'");
    };
    if path.trim().is_empty() {
        return err("write_file requires a non-empty 'path'");
    }
    let abs = container.resolve(path);
    let previous = match read_text(container, config, &abs, cancel).await {
        Ok(p) => p,
        Err(e) => return err(e),
    };
    if matches!(&previous, Existing::Text(t) if t == content) {
        return (format!("{abs} already has this content; nothing changed."), false);
    }
    if let Err(e) = write_text(container, config, &abs, content, cancel).await {
        return err(e);
    }
    let lines = count_lines(content);
    match previous {
        Existing::Missing => {
            let diff = unified_diff_async(path, "", content).await;
            (format!("Created {abs} ({lines} lines).\n{diff}"), false)
        }
        Existing::Text(old) => {
            let diff = unified_diff_async(path, &old, content).await;
            (format!("Wrote {abs} ({lines} lines).\n{diff}"), false)
        }
        Existing::Binary => (format!("Replaced binary file {abs} with text ({lines} lines)."), false),
    }
}

// ---------------------------------------------------------------------------
// grep / glob / ls
// ---------------------------------------------------------------------------

/// Keep at most `MAX_LISTING_LINES` lines and `MAX_TOOL_OUTPUT` chars.
fn cap_listing(text: &str, what: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines[..lines.len().min(MAX_LISTING_LINES)].join("\n");
    if lines.len() > MAX_LISTING_LINES {
        out.push_str(&format!(
            "\n[... {} more {what}; narrow the search]",
            lines.len() - MAX_LISTING_LINES
        ));
    }
    if out.len() > MAX_TOOL_OUTPUT {
        out = truncate_output(&out);
    }
    out
}

/// Search file contents with ripgrep (or grep when rg is missing).
pub async fn grep(
    input: &serde_json::Value,
    config: &AppConfig,
    container: &Container,
    cancel: &CancellationToken,
) -> ToolResult {
    let Some(pattern) = str_arg(input, "pattern").filter(|p| !p.is_empty()) else {
        return err("grep requires a non-empty 'pattern'");
    };
    let path = container.resolve(str_arg(input, "path").unwrap_or("."));
    let glob = str_arg(input, "glob").unwrap_or("");
    let ignore_case = if input["ignore_case"].as_bool().unwrap_or(false) { "1" } else { "0" };
    let context = input["context"].as_u64().unwrap_or(0).min(10).to_string();
    let files_only = if input["files_only"].as_bool().unwrap_or(false) { "1" } else { "0" };
    let script = r#"p="$1"; d="$2"; g="$3"; ic="$4"; c="$5"; fo="$6"
if command -v rg >/dev/null 2>&1; then
  set -- --line-number --no-heading --color=never
  if [ "$fo" = 1 ]; then set -- --files-with-matches; elif [ "$c" != 0 ]; then set -- "$@" -C "$c"; fi
  [ -n "$g" ] && set -- "$@" --glob "$g"
  [ "$ic" = 1 ] && set -- "$@" --ignore-case
  rg "$@" -e "$p" -- "$d"
else
  # Without rg there is no .gitignore: skip the usual heavy and private
  # directories by name. --include matches base names only.
  set -- -rnE --binary-files=without-match
  if [ "$fo" = 1 ]; then set -- -rlE --binary-files=without-match; elif [ "$c" != 0 ]; then set -- "$@" -C "$c"; fi
  for x in .git .syl target node_modules .venv __pycache__ dist build; do set -- "$@" --exclude-dir="$x"; done
  [ -n "$g" ] && set -- "$@" --include="${g##*/}"
  [ "$ic" = 1 ] && set -- "$@" -i
  grep "$@" -e "$p" -- "$d"
fi"#;
    let argv = ["/bin/sh", "-c", script, "sh", pattern, &path, glob, ignore_case, &context, files_only];
    match run(container, config, &argv, None, cancel).await {
        Ok(out) if out.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let shown = strip_prefix_lines(&text, container.workdir());
            (cap_listing(&shown, "lines"), false)
        }
        // Both rg and grep exit 1 for "no matches".
        Ok(out) if out.code == Some(1) && out.stderr.trim().is_empty() => ("No matches.".to_string(), false),
        // Exit 2 with output: some files couldn't be read, others matched.
        Ok(out) if out.code == Some(2) && !out.stdout.is_empty() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let shown = strip_prefix_lines(&text, container.workdir());
            let note = crate::util::truncate_output(out.stderr.trim());
            (format!("{}\n[some files could not be searched: {note}]", cap_listing(&shown, "lines")), false)
        }
        Ok(out) => err(format!("grep failed: {}", out.stderr.trim())),
        Err(e) => err(e),
    }
}

/// Show paths relative to the workspace, which is shorter and what the
/// other tools accept.
fn strip_prefix_lines(text: &str, workdir: &str) -> String {
    let prefix = format!("{}/", workdir.trim_end_matches('/'));
    text.lines()
        .map(|l| l.strip_prefix(&prefix).unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// List files matching a glob such as `src/**/*.rs` (bash globstar; hidden
/// files and directories are skipped unless the pattern names them).
pub async fn glob(
    input: &serde_json::Value,
    config: &AppConfig,
    container: &Container,
    cancel: &CancellationToken,
) -> ToolResult {
    let Some(pattern) = str_arg(input, "pattern").filter(|p| !p.trim().is_empty()) else {
        return err("glob requires a non-empty 'pattern'");
    };
    let base = container.resolve(str_arg(input, "path").unwrap_or("."));
    // Each pattern is expanded unquoted, which globs it; with IFS empty it
    // is never split into words, and expansion never runs commands. A word
    // with no glob characters comes back as itself, so only existing paths
    // are printed. Braces are expanded here: bash doesn't brace-expand the
    // result of a variable.
    let script = r#"cd -- "$1" || exit 2
shift
shopt -s globstar nullglob extglob
IFS=
for p in "$@"; do for f in $p; do [ -e "$f" ] || [ -L "$f" ] && printf '%s\n' "$f"; done; done"#;
    let patterns = expand_braces(pattern);
    let mut argv: Vec<&str> = vec!["/bin/bash", "-c", script, "bash", &base];
    argv.extend(patterns.iter().map(String::as_str));
    match run(container, config, &argv, None, cancel).await {
        Ok(out) if out.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            let mut seen = std::collections::HashSet::new();
            let text: String = raw.lines().filter(|l| seen.insert(*l)).map(|l| format!("{l}\n")).collect();
            if text.trim().is_empty() {
                (format!("No files match {pattern} in {base}."), false)
            } else {
                (cap_listing(&text, "paths"), false)
            }
        }
        Ok(out) => err(format!("glob failed: {}", out.stderr.trim())),
        Err(e) => err(e),
    }
}

/// Expand `{a,b}` alternatives (nested too) into separate patterns.
fn expand_braces(pattern: &str) -> Vec<String> {
    let bytes = pattern.as_bytes();
    let Some(open) = pattern.find('{') else { return vec![pattern.to_string()] };
    let mut depth = 0;
    let mut close = None;
    let mut commas = Vec::new();
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            b',' if depth == 1 => commas.push(i),
            _ => {}
        }
    }
    let Some(close) = close.filter(|_| !commas.is_empty()) else {
        return vec![pattern.to_string()];
    };
    let (pre, post) = (&pattern[..open], &pattern[close + 1..]);
    let mut bounds = vec![open];
    bounds.extend(commas);
    bounds.push(close);
    let mut out = Vec::new();
    for w in bounds.windows(2) {
        let alt = &pattern[w[0] + 1..w[1]];
        for tail in expand_braces(&format!("{alt}{post}")) {
            out.push(format!("{pre}{tail}"));
        }
        if out.len() > 64 {
            break;
        }
    }
    out
}

/// List a directory.
pub async fn ls(
    input: &serde_json::Value,
    config: &AppConfig,
    container: &Container,
    cancel: &CancellationToken,
) -> ToolResult {
    let path = container.resolve(str_arg(input, "path").unwrap_or("."));
    match run(container, config, &["ls", "-lAh", "--", &path], None, cancel).await {
        Ok(out) if out.success() => (cap_listing(&String::from_utf8_lossy(&out.stdout), "entries"), false),
        Ok(out) => err(format!("ls failed: {}", out.stderr.trim())),
        Err(e) => err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_has_headers_and_hunks() {
        let d = unified_diff("a.txt", "one\ntwo\nthree\n", "one\n2\nthree\n");
        assert!(d.starts_with("--- a/a.txt\n+++ b/a.txt\n@@"), "{d}");
        assert!(d.contains("-two\n+2\n"), "{d}");
    }

    #[test]
    fn braces_expand() {
        assert_eq!(expand_braces("src/*.{ts,tsx}"), ["src/*.ts", "src/*.tsx"]);
        assert_eq!(expand_braces("{a,b/{c,d}}.rs"), ["a.rs", "b/c.rs", "b/d.rs"]);
        assert_eq!(expand_braces("plain{}.rs"), ["plain{}.rs"]);
    }

    #[test]
    fn listing_is_capped() {
        let many: String = (0..1000).map(|i| format!("f{i}\n")).collect();
        let out = cap_listing(&many, "paths");
        assert!(out.contains("700 more paths"));
        assert_eq!(out.lines().count(), MAX_LISTING_LINES + 1);
    }
}
