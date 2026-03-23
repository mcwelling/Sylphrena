pub const MAX_TOOL_OUTPUT: usize = 8000;

pub fn truncate_output(s: &str) -> String {
    if s.len() > MAX_TOOL_OUTPUT {
        format!("{}\n... [truncated: {} total chars]", &s[..MAX_TOOL_OUTPUT], s.len())
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
    static RE_SCRIPT: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?si)<(script|style)[^>]*>.*?</\1>").unwrap());
    static RE_TAGS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"<[^>]+>").unwrap());
    static RE_WS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\s+").unwrap());

    let text = RE_SCRIPT.replace_all(text, " ");
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
