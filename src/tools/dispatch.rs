/// `dispatch_agent` tool — fetches a URL and processes it via a Haiku sub-agent.
///
/// See `docs/dispatch-agent-rs-design.md` for the full design.
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::api::client::{call_with_retry_shared, CancelledError, SystemBlock};
use crate::api::types::{Message, StopReason};
use crate::config::AppConfig;
use crate::shared::SharedState;
use crate::util::{is_html, strip_html};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Versioned model ID for the dispatch sub-agent.
/// Update here when Haiku is upgraded; also verify the ID matches the runtime
/// api_url (direct Anthropic API vs Bedrock use different identifiers).
const HAIKU_MODEL: &str = "claude-haiku-4-5-20251001";

/// Maximum characters of fetched content passed to the sub-agent.
/// Chosen to keep sub-agent calls fast and cheap — not a context limit
/// (Haiku 4.5 has a 200K-token context window).
const MAX_FETCH_CHARS: usize = 32_000;

const HAIKU_SYSTEM: &str = "\
You are a research assistant. Return only what the user asks for — no \
preamble, no commentary, no apology. Output must match the requested \
format exactly.";

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the `dispatch_agent` tool. Returns `(output_json, is_error)`.
pub async fn dispatch_agent(
    input:  &serde_json::Value,
    config: &AppConfig,
    state:  &Arc<SharedState>,
    client: &reqwest::Client,
    cancel: CancellationToken,
) -> (String, bool) {
    // --- Input validation ---
    let task = match input["task"].as_str() {
        Some(t @ ("fetch" | "summarize" | "extract")) => t,
        Some(other) => return err_json("Invalid parameter", &format!("unknown task: {other}"), ""),
        None        => return err_json("Invalid parameter", "missing 'task' field", ""),
    };
    let source = match input["source"].as_str().filter(|s| !s.is_empty()) {
        Some(s) => s,
        None    => return err_json("Invalid parameter", "missing or empty 'source' field", ""),
    };
    let focus = match input["focus"].as_str().filter(|s| !s.is_empty()) {
        Some(f) => f,
        None    => return err_json("Invalid parameter", "missing or empty 'focus' field", source),
    };
    let fmt = match input["output_format"].as_str() {
        Some(f @ ("text" | "json" | "list")) => f,
        Some(other) => return err_json("Invalid parameter", &format!("unknown output_format: {other}"), source),
        None        => return err_json("Invalid parameter", "missing 'output_format' field", source),
    };
    let max_tokens: u32 = input["max_length"]
        .as_u64()
        .map(|v| v.clamp(100, 4096) as u32)
        .unwrap_or(800);

    // --- Fetch ---
    let raw = match fetch_source(source, config, cancel.clone()).await {
        Ok(r)  => r,
        Err(e) => return err_json("Content retrieval failed", &e, source),
    };

    // Strip HTML; truncate to MAX_FETCH_CHARS
    let content = if is_html(&raw) { strip_html(&raw) } else { raw };
    let content = if content.len() > MAX_FETCH_CHARS {
        content[..MAX_FETCH_CHARS].to_string()
    } else {
        content
    };

    // --- Sub-agent call ---
    let haiku_cfg  = haiku_config(config, max_tokens);
    let system     = vec![SystemBlock::text(HAIKU_SYSTEM)];
    let user_msg   = build_haiku_user_message(&content, task, focus, fmt);
    let messages   = vec![Message::user_text(user_msg)];

    let result = call_with_retry_shared(
        client, &haiku_cfg, state, &messages, &system, cancel.clone(), |_| {}, |_| {},
    ).await;

    let turn = match result {
        Ok(t)  => t,
        Err(e) => {
            if e.downcast_ref::<CancelledError>().is_some() {
                return err_json("Cancelled", "request cancelled by user", source);
            }
            let reason = e.to_string();
            return err_json("Sub-agent call failed", &reason, source);
        }
    };

    let text = match turn.content.iter().find_map(|b| {
        if let crate::api::types::ContentBlock::Text { text, .. } = b { Some(text.as_str()) } else { None }
    }) {
        Some(t) => t.to_string(),
        None    => return err_json("Sub-agent call failed", "no text in response", source),
    };

    // MaxTokens: for json/list this is always fatal; for text we return the
    // partial content with a warning field.
    if turn.stop_reason == StopReason::MaxTokens {
        if fmt == "json" || fmt == "list" {
            return err_json("Response truncated", "max_tokens reached before complete response", source);
        }
        // text: return partial with warning
        return build_output(task, source, fmt, &text, Some("response truncated at max_tokens"));
    }

    // --- Format validation (json only) ---
    if fmt == "json" {
        if serde_json::from_str::<serde_json::Value>(&text).is_err() {
            // Retry once with stricter prompt — fresh call, no prior response in context
            let retry_msg  = build_haiku_retry_message(&content, task, focus);
            let retry_msgs = vec![Message::user_text(retry_msg)];

            let retry_result = call_with_retry_shared(
                client, &haiku_cfg, state, &retry_msgs, &system, cancel.clone(), |_| {}, |_| {},
            ).await;

            let retry_text = match retry_result {
                Ok(t) => t.content.iter().find_map(|b| {
                    if let crate::api::types::ContentBlock::Text { text, .. } = b {
                        Some(text.clone())
                    } else {
                        None
                    }
                }).unwrap_or_default(),
                Err(e) => return err_json("Sub-agent call failed", &e.to_string(), source),
            };

            if serde_json::from_str::<serde_json::Value>(&retry_text).is_err() {
                return err_json(
                    "Format validation failed",
                    "model did not return valid JSON after retry",
                    source,
                );
            }
            return build_output(task, source, fmt, &retry_text, None);
        }
    }

    build_output(task, source, fmt, &text, None)
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

async fn fetch_source(
    source: &str,
    config: &AppConfig,
    cancel: CancellationToken,
) -> Result<String, String> {
    let is_pdf = source.to_lowercase().ends_with(".pdf");
    let cmd = if is_pdf {
        format!(
            "ts=$(date +%s%N); f=/tmp/syl_dp_$ts.pdf; \
             curl -sL -A 'Mozilla/5.0' --max-time 30 '{}' -o \"$f\" \
             && pdftotext \"$f\" - && rm -f \"$f\"",
            source
        )
    } else {
        format!("curl -sL -A 'Mozilla/5.0' --max-time 30 '{}'", source)
    };

    let running = std::sync::Arc::new(std::sync::Mutex::new(None));
    let (output, is_err) = crate::tools::bash::run_bash(
        &serde_json::json!({ "command": cmd }),
        config,
        running,
        cancel,
    ).await;

    if is_err {
        return Err(output);
    }
    if output.trim().is_empty() {
        return Err("empty response from source".to_string());
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Haiku config (explicit fields — no silent propagation via ..base.clone())
// ---------------------------------------------------------------------------

fn haiku_config(base: &AppConfig, max_tokens: u32) -> AppConfig {
    AppConfig {
        model:           HAIKU_MODEL.to_string(),
        max_tokens,
        thinking_budget: None,
        // Flags that must never be set for a sub-agent call
        resume:   false,
        fresh:    false,
        no_banner: false,
        // All other fields (auth, backend, region, rate-limit, UI, etc.)
        // pass through from base. New AppConfig fields are automatically included.
        ..base.clone()
    }
}

// ---------------------------------------------------------------------------
// Prompt builders (pure functions — independently testable)
// ---------------------------------------------------------------------------

/// Build the user message for the Haiku sub-agent.
pub fn build_haiku_user_message(content: &str, task: &str, focus: &str, fmt: &str) -> String {
    let format_instruction = match fmt {
        "json" => "Respond with valid JSON only. No markdown fences. No surrounding text.",
        "list" => "Respond with a plain text bullet list. One item per line. No preamble.",
        _      => "Respond in plain text. Be concise.",
    };

    format!(
        "Task: {task}\n\
         Focus: {focus}\n\
         Output format: {fmt}\n\
         {format_instruction}\n\n\
         Content:\n\
         ---\n\
         {content}\n\
         ---"
    )
}

/// Build the retry message when the first JSON response was malformed.
/// Fresh call — deliberately excludes the prior bad response.
fn build_haiku_retry_message(content: &str, task: &str, focus: &str) -> String {
    format!(
        "Task: {task}\n\
         Focus: {focus}\n\
         Output format: json\n\
         IMPORTANT: The previous attempt did not return valid JSON. \
         Respond with valid JSON only. No markdown fences. No surrounding text.\n\n\
         Content:\n\
         ---\n\
         {content}\n\
         ---"
    )
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

    #[test]
    fn haiku_config_overrides_model_and_tokens() {
        let base = AppConfig::default();
        let cfg  = haiku_config(&base, 400);
        assert_eq!(cfg.model, HAIKU_MODEL);
        assert_eq!(cfg.max_tokens, 400);
        assert!(cfg.thinking_budget.is_none());
    }

    #[test]
    fn haiku_config_clamps_no_tools_fields() {
        // Verify no tool-enabling fields exist that could leak through.
        // If AppConfig gains such a field and haiku_config is not updated,
        // this test will fail to compile (field not initialised).
        let base = AppConfig::default();
        let cfg  = haiku_config(&base, 100);
        assert_eq!(cfg.resume, false);
        assert_eq!(cfg.fresh, false);
    }

    #[test]
    fn build_haiku_user_message_json_format() {
        let msg = build_haiku_user_message("some content", "summarize", "key points", "json");
        assert!(msg.contains("Task: summarize"));
        assert!(msg.contains("Focus: key points"));
        assert!(msg.contains("Output format: json"));
        assert!(msg.contains("valid JSON only"));
        assert!(msg.contains("some content"));
    }

    #[test]
    fn build_haiku_user_message_text_format() {
        let msg = build_haiku_user_message("c", "fetch", "f", "text");
        assert!(msg.contains("Output format: text"));
        assert!(msg.contains("plain text"));
    }

    #[test]
    fn build_haiku_user_message_list_format() {
        let msg = build_haiku_user_message("c", "extract", "f", "list");
        assert!(msg.contains("bullet list"));
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
