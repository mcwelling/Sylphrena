pub const MAX_TOOL_OUTPUT: usize = 8000;

pub fn truncate_output(s: &str) -> String {
    if s.len() > MAX_TOOL_OUTPUT {
        // Back off to a char boundary so multi-byte UTF-8 never panics the slice.
        let mut cut = MAX_TOOL_OUTPUT;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n... [truncated: {} total chars]", &s[..cut], s.len())
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// HTML detection and stripping (shared by run_bash and dispatch_agent)
// ---------------------------------------------------------------------------

pub fn is_html(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("<!doctype html") || lower.contains("<html") || lower.contains("<body")
}

pub fn strip_html(text: &str) -> String {
    use std::sync::LazyLock;
    // The regex crate has no backreferences (`</\1>` panics on first use),
    // so script and style blocks get one pattern each.
    static RE_SCRIPT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?si)<script[^>]*>.*?</script\s*>").unwrap());
    static RE_STYLE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?si)<style[^>]*>.*?</style\s*>").unwrap());
    static RE_TAGS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap());
    static RE_WS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\s+").unwrap());

    let text = RE_SCRIPT.replace_all(text, " ");
    let text = RE_STYLE.replace_all(&text, " ");
    let text = RE_TAGS.replace_all(&text, " ");
    RE_WS.replace_all(&text, " ").trim().to_string()
}

// ---------------------------------------------------------------------------
// Three-stage tool output processing (mirrors syl.bb process-tool-output)
// ---------------------------------------------------------------------------

/// Process raw tool output for insertion into the model's context window.
/// 1. HTML detection → strip tags then truncate.
/// 2. Large output (> 100 lines) → numbered 100-line preview with hint.
/// 3. Final hard truncation at MAX_TOOL_OUTPUT chars.
pub fn process_tool_output(raw: &str) -> String {
    // Stage 1: HTML
    if is_html(raw) {
        return truncate_output(&strip_html(raw));
    }

    // Stage 2: Paged output
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() > 100 {
        let total = lines.len();
        let preview = lines[..100]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>5}: {}", i + 1, l))
            .collect::<Vec<_>>()
            .join("\n");
        return format!(
            "[Output paged: {total} lines total, showing lines 1-100]\n\
             [Use read_file for file content, or narrow with head/grep/sed for command output]\n\
             \n\
             {preview}"
        );
    }

    // Stage 3: Hard truncation
    truncate_output(raw)
}

/// Where `run_bash` saves output too large to show the model: a directory
/// on the host and the same directory as seen inside the container.
#[derive(Debug, Clone)]
pub struct SpillDir {
    pub host: std::path::PathBuf,
    pub container: String,
}

/// Lines longer than this are wrapped in a spill file, so that
/// `read_file`, which pages by line, can reach all of it.
pub const SPILL_LINE_CHARS: usize = 2_000;

fn wrap_long_lines(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let mut rest = body;
        while rest.len() > max {
            let mut cut = max;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push_str(&rest[..cut]);
            out.push('\n');
            rest = &rest[cut..];
        }
        out.push_str(rest);
        out.push_str(nl);
    }
    out
}

/// `process_tool_output`, except that output too long to show in full is
/// saved to `<spill>/<name>.txt` and the model gets a preview plus the path,
/// which it can page with `read_file`. The file holds the raw output (HTML
/// included, so it keeps its lines), with lines over `SPILL_LINE_CHARS`
/// wrapped. Without a spill directory, or if the file can't be written, the
/// output is cut as `process_tool_output` does.
pub fn process_tool_output_spilled(raw: &str, spill: Option<&SpillDir>, name: &str) -> String {
    let html = is_html(raw);
    let shown_text = if html { strip_html(raw) } else { raw.to_string() };
    if shown_text.lines().count() <= 100 && shown_text.len() <= MAX_TOOL_OUTPUT {
        return shown_text;
    }
    let Some(spill) = spill else {
        return process_tool_output(raw);
    };
    let file: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let file = format!("{}.txt", if file.is_empty() { "output".to_string() } else { file });
    let saved = wrap_long_lines(raw, SPILL_LINE_CHARS);
    let host = spill.host.join(&file);
    let written = std::fs::create_dir_all(&spill.host).and_then(|()| std::fs::write(&host, &saved));
    if let Err(e) = written {
        tracing::warn!(path = %host.display(), "could not save large tool output: {e}");
        return process_tool_output(raw);
    }
    let lines: Vec<&str> = saved.lines().collect();
    let (what, preview) = if html {
        ("Showing the page as text".to_string(), truncate_output(&shown_text))
    } else {
        let shown = lines.len().min(100);
        let numbered = lines[..shown]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>5}: {}", i + 1, l))
            .collect::<Vec<_>>()
            .join("\n");
        (format!("Showing lines 1-{shown}"), truncate_output(&numbered))
    };
    format!(
        "[Output: {} lines, {} chars. {what}. The full output is saved at {}/{file} \
         (lines over {SPILL_LINE_CHARS} chars wrapped); page through it with read_file \
         (start_line/end_line) or search it with grep.]\n\n{preview}",
        lines.len(),
        raw.len(),
        spill.container.trim_end_matches('/'),
    )
}

pub use syl_container::shell_quote;

/// Validate a memory / workflow key before it becomes a file name on the HOST.
/// Allows ASCII letters, digits, `-`, `_` and `.`; rejects empty keys, leading
/// dots, and `..` so a key can never escape its directory.
pub fn is_safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && !key.starts_with('.')
        && !key.contains("..")
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn fmt_num(n: u64) -> String {
    if n == 0 {
        "0".to_string()
    } else if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}
