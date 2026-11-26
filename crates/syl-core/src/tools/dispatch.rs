//! `dispatch_agent` and `fetch`.
//!
//! `dispatch_agent` runs the built-in `fetch` subagent (see `agents.rs`):
//! this module turns its fields into the subagent's prompt, and its reply
//! into `dispatch_agent`'s JSON result. `fetch` is that subagent's tool: it
//! gets a page in the container and returns it a window at a time. See the
//! "Output Handling" section of `docs/architecture.md`.

use tokio_util::sync::CancellationToken;

use syl_container::Container;

use crate::config::AppConfig;
use crate::util::{is_html, strip_html};

/// Most characters of a page one `fetch` call returns.
pub const MAX_FETCH_CHARS: usize = 32_000;

/// `dispatch_agent`'s fields, checked.
pub struct Dispatch {
    pub task: String,
    pub source: String,
    pub focus: String,
    pub fmt: String,
    /// Tokens the reply may use.
    pub max_tokens: u32,
}

/// Check `dispatch_agent`'s input; Err is the tool result to return.
pub fn parse_dispatch(input: &serde_json::Value) -> Result<Dispatch, (String, bool)> {
    let task = match input["task"].as_str() {
        Some(t @ ("fetch" | "summarize" | "extract")) => t,
        Some(other) => return Err(err_json("Invalid parameter", &format!("unknown task: {other}"), "")),
        None => return Err(err_json("Invalid parameter", "missing 'task' field", "")),
    };
    let Some(source) = input["source"].as_str().filter(|s| !s.is_empty()) else {
        return Err(err_json("Invalid parameter", "missing or empty 'source' field", ""));
    };
    let Some(focus) = input["focus"].as_str().filter(|s| !s.is_empty()) else {
        return Err(err_json("Invalid parameter", "missing or empty 'focus' field", source));
    };
    let fmt = match input["output_format"].as_str() {
        Some(f @ ("text" | "json" | "list")) => f,
        Some(other) => return Err(err_json("Invalid parameter", &format!("unknown output_format: {other}"), source)),
        None => return Err(err_json("Invalid parameter", "missing 'output_format' field", source)),
    };
    let max_tokens = input["max_length"].as_u64().map(|v| v.clamp(100, 4096) as u32).unwrap_or(800);
    Ok(Dispatch {
        task: task.to_string(),
        source: source.to_string(),
        focus: focus.to_string(),
        fmt: fmt.to_string(),
        max_tokens,
    })
}

impl Dispatch {
    /// The `fetch` subagent's prompt.
    pub fn prompt(&self) -> String {
        let format_instruction = match self.fmt.as_str() {
            "json" => "Respond with valid JSON only. No markdown fences. No surrounding text.",
            "list" => "Respond with a plain text bullet list. One item per line. No preamble.",
            _ => "Respond in plain text. Be concise.",
        };
        let what = match self.task.as_str() {
            "summarize" => "Summarise it: a summary and its key points.",
            "extract" => "Extract the data asked for, as a list of items.",
            _ => "Return the relevant excerpt.",
        };
        format!(
            "Fetch {} and answer from it.\nTask: {} ({what})\nFocus: {}\nOutput format: {}\n{format_instruction}\n\
             Keep the answer under about {} tokens.",
            self.source, self.task, self.focus, self.fmt, self.max_tokens
        )
    }

    /// Told to the subagent when its JSON didn't parse.
    pub const RETRY_JSON: &'static str =
        "That reply was not valid JSON. Reply again with valid JSON only: no markdown fences, no surrounding text.";

    /// Whether `reply` is the format asked for: for json, it must parse.
    pub fn accepts(&self, reply: &str) -> bool {
        self.fmt != "json" || serde_json::from_str::<serde_json::Value>(reply).is_ok()
    }

    /// The tool result for the subagent's final reply. `truncated` is a
    /// reply cut off at max_tokens.
    pub fn finish(&self, reply: &str, truncated: bool) -> (String, bool) {
        if reply.trim().is_empty() {
            return err_json("Sub-agent call failed", "no text in response", &self.source);
        }
        if !self.accepts(reply) {
            let why = if truncated { "max_tokens reached before complete response" } else { "model did not return valid JSON after retry" };
            let what = if truncated { "Response truncated" } else { "Format validation failed" };
            return err_json(what, why, &self.source);
        }
        if truncated && self.fmt == "list" {
            return err_json("Response truncated", "max_tokens reached before complete response", &self.source);
        }
        build_output(&self.task, &self.source, &self.fmt, reply, truncated.then_some("response truncated at max_tokens"))
    }
}

/// The `fetch` tool: the page at `url`, as text, from `start` (a byte
/// offset), at most [`MAX_FETCH_CHARS`] of it, saying where to go on.
pub async fn fetch(input: &serde_json::Value, config: &AppConfig, container: &Container, cancel: CancellationToken) -> (String, bool) {
    let Some(url) = input["url"].as_str().filter(|u| !u.trim().is_empty()) else {
        return ("[Tool error: fetch requires a 'url']".to_string(), true);
    };
    let raw = match fetch_source(url, config, container, cancel).await {
        Ok(r) => r,
        Err(e) => return (format!("[Tool error: could not fetch {url}: {e}]"), true),
    };
    let content = if is_html(&raw) { strip_html(&raw) } else { raw };
    (window(&content, input["start"].as_u64().unwrap_or(0) as usize), false)
}

/// `content` from `start`, cut to [`MAX_FETCH_CHARS`] on character
/// boundaries, with a note on what was left out.
pub fn window(content: &str, start: usize) -> String {
    let mut from = start.min(content.len());
    while !content.is_char_boundary(from) {
        from -= 1;
    }
    let mut to = (from + MAX_FETCH_CHARS).min(content.len());
    while !content.is_char_boundary(to) {
        to -= 1;
    }
    let mut out = content[from..to].to_string();
    if from > 0 || to < content.len() {
        out.push_str(&format!("\n[... cut here: bytes {from}-{to} of {} bytes shown", content.len()));
        if to < content.len() {
            out.push_str(&format!("; call fetch again with start={to} for more"));
        }
        out.push(']');
    }
    out
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

async fn fetch_source(
    source:    &str,
    config:    &AppConfig,
    container: &Container,
    cancel:    CancellationToken,
) -> Result<String, String> {
    let is_pdf = source.to_lowercase().ends_with(".pdf");
    let cmd = if is_pdf {
        format!(
            "ts=$(date +%s%N); f=/tmp/syl_dp_$ts.pdf; \
             curl -sL -A 'Mozilla/5.0' --max-time 30 {} -o \"$f\" \
             && pdftotext \"$f\" - && rm -f \"$f\"",
            crate::util::shell_quote(source)
        )
    } else {
        format!("curl -sL -A 'Mozilla/5.0' --max-time 30 {}", crate::util::shell_quote(source))
    };

    // Run curl directly rather than through run_bash: run_bash shapes its
    // output for the main model (100-line previews, a read_file hint), which
    // would cut the page short and confuse the sub-agent. Truncation happens
    // in dispatch_agent instead.
    let timeout = std::time::Duration::from_millis(config.timeout_ms);
    let output = match container.exec(&cmd, timeout, &cancel).await {
        Ok(out) if out.success() => out.stdout,
        Ok(out) => return Err(out.error_text()),
        Err(syl_container::ExecError::TimedOut(_)) => {
            return Err(format!("fetch timed out after {}s", config.timeout_ms / 1000))
        }
        Err(e) => return Err(e.to_string()),
    };
    if output.trim().is_empty() {
        return Err("empty response from source".to_string());
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Output assembly
// ---------------------------------------------------------------------------

/// Assemble the final success JSON, injecting `source_url` and an optional
/// `warning` field. The task-specific key wraps the sub-agent text.
///
/// All three task types always return an outer JSON object so the caller
/// never needs to branch on content type:
///
/// | task      | output_format=json              | output_format=text/list      |
/// |-----------|----------------------------------|-------------------------------|
/// | fetch     | parsed JSON in "content"         | text string in "content"      |
/// | summarize | parsed JSON in "summary"         | text string in "summary"      |
/// | extract   | parsed JSON in "data"            | text string in "data"         |
fn build_output(task: &str, source: &str, fmt: &str, text: &str, warning: Option<&str>) -> (String, bool) {
    let inner: serde_json::Value = if fmt == "json" {
        // Already validated — safe to unwrap
        serde_json::from_str(text).unwrap_or(serde_json::Value::String(text.to_string()))
    } else {
        serde_json::Value::String(text.to_string())
    };

    let field = match task {
        "summarize" => "summary",
        "extract"   => "data",
        _           => "content",   // fetch (and any future task type)
    };

    let mut out = serde_json::json!({
        field: inner,
        "source_url": source,
    });

    if let Some(w) = warning {
        out["warning"] = serde_json::Value::String(w.to_string());
    }

    (out.to_string(), false)
}

// ---------------------------------------------------------------------------
// Error helper
// ---------------------------------------------------------------------------

fn err_json(error: &str, reason: &str, source_url: &str) -> (String, bool) {
    (serde_json::json!({
        "error": error,
        "reason": reason,
        "source_url": source_url,
    }).to_string(), true)
}

// HTML helpers are in crate::util (is_html, strip_html)

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(fmt: &str) -> Dispatch {
        parse_dispatch(&serde_json::json!({
            "task": "summarize", "source": "http://x.com", "focus": "key points", "output_format": fmt, "max_length": 99999
        }))
        .ok()
        .unwrap()
    }

    #[test]
    fn the_prompt_carries_the_fields() {
        let d = dispatch("json");
        assert_eq!(d.max_tokens, 4096);
        let p = d.prompt();
        assert!(p.contains("Fetch http://x.com") && p.contains("Task: summarize") && p.contains("Focus: key points"));
        assert!(p.contains("valid JSON only"));
        assert!(dispatch("list").prompt().contains("bullet list"));
        assert!(dispatch("text").prompt().contains("plain text"));
    }

    #[test]
    fn bad_input_is_an_error_result() {
        let (s, is_err) = parse_dispatch(&serde_json::json!({"task": "fly"})).err().unwrap();
        assert!(is_err && s.contains("unknown task"));
    }

    #[test]
    fn finish_checks_the_format() {
        assert!(!dispatch("json").accepts("not json"));
        let (s, err) = dispatch("json").finish("{\"a\":1}", false);
        assert!(!err && s.contains("\"summary\":{\"a\":1}"));
        assert!(dispatch("json").finish("nope", false).1);
        let (s, err) = dispatch("text").finish("partial", true);
        assert!(!err && s.contains("truncated"));
        assert!(dispatch("list").finish("- a", true).1);
        assert!(dispatch("text").finish("  ", false).1);
    }

    #[test]
    fn fetch_windows_on_character_boundaries() {
        let page = "日本語".repeat(20_000);
        let first = window(&page, 0);
        assert!(first.contains("call fetch again with start="), "{}", &first[first.len() - 120..]);
        assert!(first.contains(&format!("of {} bytes shown", page.len())));
        let last = window(&page, page.len() - 10);
        assert!(!last.contains("call fetch again"));
        assert_eq!(window("short", 0), "short");
    }

    #[test]
    fn format_validation_json_pass() {
        assert!(serde_json::from_str::<serde_json::Value>("{\"a\":1}").is_ok());
    }

    #[test]
    fn format_validation_json_fail() {
        assert!(serde_json::from_str::<serde_json::Value>("not json").is_err());
    }

    #[test]
    fn build_output_fetch_json() {
        let (s, err) = build_output("fetch", "http://x.com", "json", "{\"k\":1}", None);
        assert!(!err);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["source_url"], "http://x.com");
        assert_eq!(v["content"]["k"], 1);
        assert!(v.get("warning").is_none());
    }

    #[test]
    fn build_output_summarize_text() {
        let (s, err) = build_output("summarize", "http://x.com", "text", "plain summary", None);
        assert!(!err);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["summary"], "plain summary");
        assert_eq!(v["source_url"], "http://x.com");
    }

    #[test]
    fn build_output_extract_list() {
        let (s, err) = build_output("extract", "http://x.com", "list", "- item1\n- item2", None);
        assert!(!err);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["data"], "- item1\n- item2");
    }

    #[test]
    fn build_output_with_warning() {
        let (s, _) = build_output("fetch", "http://x.com", "text", "partial", Some("response truncated at max_tokens"));
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["warning"], "response truncated at max_tokens");
    }

    #[test]
    fn err_json_structure() {
        let (s, is_err) = err_json("Content retrieval failed", "empty response", "http://x.com");
        assert!(is_err);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["error"], "Content retrieval failed");
        assert_eq!(v["reason"], "empty response");
        assert_eq!(v["source_url"], "http://x.com");
    }
}
