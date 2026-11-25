/// Async HTTP API client for the Anthropic Messages API.
/// `stream_turn` sends a streaming POST and reassembles the SSE into a `TurnResult`.
/// `call_with_retry_shared` wraps it with rate-limit throttle + 429/529 retry.
use anyhow::Context;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use tokio_util::sync::CancellationToken;

use crate::api::sse::StreamHandler;
use crate::api::types::{Message, TurnResult};
use crate::config::{AppConfig, ANTHROPIC_VERSION};

// ---------------------------------------------------------------------------
// Typed cancellation error
// ---------------------------------------------------------------------------

/// Returned when a request is cancelled via `CancellationToken`.
/// Using a typed error lets callers distinguish cancellation from other failures
/// without string matching on the error message.
#[derive(Debug)]
pub struct CancelledError;

impl std::fmt::Display for CancelledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}

impl std::error::Error for CancelledError {}

// ---------------------------------------------------------------------------
// Retry config
// ---------------------------------------------------------------------------

/// Retries after a 429 / 529. With `retry-after` the API says how long; without
/// it the waits double from 1s to at most 30s (about a minute and a half in all).
const MAX_RETRIES: u32 = 6;
const BASE_DELAY_MS: u64 = 1000;
const MAX_BACKOFF_MS: u64 = 30_000;
/// The longest Syl waits on the API's say-so (`retry-after`, or a bucket
/// refilling) before a request.
const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// A 429 (rate limited) or 529 (overloaded), before or during the stream.
/// Its text starts with `rate_limit:` like the errors it replaces.
#[derive(Debug)]
pub struct RateLimited {
    pub status: u16,
    /// From the `retry-after` header.
    pub retry_after: Option<std::time::Duration>,
    pub detail: String,
}

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rate_limit:{}", self.status)?;
        if !self.detail.is_empty() {
            write!(f, " ({})", self.detail)?;
        }
        Ok(())
    }
}

impl std::error::Error for RateLimited {}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn auth_headers(config: &AppConfig) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-api-key"),
        HeaderValue::from_str(&config.api_key).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    headers
}

/// System block as sent in the API request (text + optional cache_control).
#[derive(Debug, Clone, serde::Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<crate::api::types::CacheControl>,
}

impl SystemBlock {
    pub fn text(text: impl Into<String>) -> Self {
        SystemBlock { kind: "text", text: text.into(), cache_control: None }
    }
    pub fn cached(text: impl Into<String>) -> Self {
        SystemBlock {
            kind: "text",
            text: text.into(),
            cache_control: Some(crate::api::types::CacheControl::ephemeral()),
        }
    }
}

fn build_payload(
    messages: &[Message],
    system: &[SystemBlock],
    config: &AppConfig,
) -> serde_json::Value {
    let max_tokens = request_max_tokens(config);

    let mut body = serde_json::json!({
        "model": config.model,
        "max_tokens": max_tokens,
        "stream": true,
        "system": system,
        "messages": messages,
    });
    if config.tools {
        let mut tools = syl_tools();
        if let Some(list) = tools.as_array_mut() {
            list.extend(config.extra_tools.iter().cloned());
        }
        body["tools"] = tools;
    }

    if let Some(thinking) = thinking_param(config) {
        body["thinking"] = thinking;
    }

    body
}

/// The `thinking` request field for `--thinking <budget>`, or None when the
/// flag isn't set.
///
/// Models with manual thinking get `{type: enabled, budget_tokens}`. Adaptive
/// models (Opus 4.7+, all 5.x) reject that with a 400 and always think anyway,
/// returning *omitted* (empty) thinking by default, so for them `--thinking`
/// asks for summarized thinking to be streamed instead.
pub fn thinking_param(config: &AppConfig) -> Option<serde_json::Value> {
    let budget = config.thinking_budget?;
    Some(if crate::config::supports_manual_thinking(&config.model) {
        serde_json::json!({ "type": "enabled", "budget_tokens": manual_thinking_budget(budget) })
    } else {
        serde_json::json!({ "type": "adaptive", "display": "summarized" })
    })
}

/// The API rejects `budget_tokens` below this.
pub const MIN_THINKING_BUDGET: u64 = 1024;

fn manual_thinking_budget(budget: u64) -> u64 {
    budget.max(MIN_THINKING_BUDGET)
}

/// `max_tokens` for a request. Manual thinking spends `budget_tokens` out of
/// `max_tokens`, so the budget is added on top to leave room for the answer.
/// Adaptive models ignore the budget, so it must not inflate `max_tokens`
/// (a large `--thinking` value would otherwise exceed the model's output cap).
pub fn request_max_tokens(config: &AppConfig) -> u64 {
    match config.thinking_budget {
        Some(budget) if crate::config::supports_manual_thinking(&config.model) => {
            config.max_tokens as u64 + manual_thinking_budget(budget)
        }
        _ => config.max_tokens as u64,
    }
}

// ---------------------------------------------------------------------------
// Bytes-stream SSE parser
// ---------------------------------------------------------------------------

/// Stateful line-buffering SSE parser for use over a raw `bytes_stream()`.
/// `feed()` accepts arbitrary byte chunks and returns complete parsed events.
/// This is necessary because HTTP/2 chunks can split at any byte boundary.
pub struct SseParser {
    buf:        Vec<u8>,
    event_type: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        SseParser { buf: Vec::new(), event_type: None }
    }

    /// Accept a chunk of bytes, return all complete SSE events extracted.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();

        loop {
            // Find next newline
            let Some(nl) = self.buf.iter().position(|&b| b == b'\n') else { break };
            let line_bytes = self.buf.drain(..=nl).collect::<Vec<u8>>();
            let line = String::from_utf8_lossy(&line_bytes);
            let line = line.trim_end_matches(['\n', '\r']);

            if line.is_empty() {
                self.event_type = None;
                continue;
            }
            if let Some(rest) = line.strip_prefix("event: ") {
                self.event_type = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("data: ") {
                if rest == "[DONE]" { continue; }
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(rest) {
                    events.push(SseEvent {
                        event_type: self.event_type.clone(),
                        data: val,
                    });
                }
            }
        }
        events
    }
}

#[derive(Debug)]
pub struct SseEvent {
    pub event_type: Option<String>,
    pub data: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Delta events emitted to the caller during streaming
// ---------------------------------------------------------------------------

/// A streaming content delta forwarded to the `on_delta` callback.
#[derive(Debug, Clone)]
pub enum BlockDeltaEvent {
    ThinkingDelta  { text: String },
    TextDelta      { text: String },
    InputJsonDelta { text: String },
}

fn extract_block_delta(data: &serde_json::Value) -> Option<BlockDeltaEvent> {
    if data["type"].as_str() != Some("content_block_delta") {
        return None;
    }
    let delta = &data["delta"];
    match delta["type"].as_str()? {
        "text_delta"      => Some(BlockDeltaEvent::TextDelta {
            text: delta["text"].as_str().unwrap_or("").to_string(),
        }),
        "thinking_delta"  => Some(BlockDeltaEvent::ThinkingDelta {
            text: delta["thinking"].as_str().unwrap_or("").to_string(),
        }),
        "input_json_delta"=> Some(BlockDeltaEvent::InputJsonDelta {
            text: delta["partial_json"].as_str().unwrap_or("").to_string(),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Core streaming function
// ---------------------------------------------------------------------------

#[tracing::instrument(skip_all, fields(model = %config.model, msgs = messages.len()))]
pub async fn stream_turn(
    client:   &reqwest::Client,
    config:   &AppConfig,
    messages: &[Message],
    system:   &[SystemBlock],
    cancel:   CancellationToken,
    on_delta: &impl Fn(BlockDeltaEvent),
) -> anyhow::Result<TurnResult> {
    stream_turn_observed(client, config, messages, system, cancel, on_delta, None).await
}

/// `stream_turn`, also storing the response's rate-limit headers in `limits`.
async fn stream_turn_observed(
    client:   &reqwest::Client,
    config:   &AppConfig,
    messages: &[Message],
    system:   &[SystemBlock],
    cancel:   CancellationToken,
    on_delta: &impl Fn(BlockDeltaEvent),
    limits:   Option<&std::sync::Mutex<Option<crate::api::rate_limit::ApiLimits>>>,
) -> anyhow::Result<TurnResult> {
    let messages = crate::api::cache::attach_cache_breakpoints(messages.to_vec());

    let payload = build_payload(&messages, system, config);

    let request = client
        .post(&config.api_url)
        .headers(auth_headers(config))
        .header("content-type", "application/json")
        .json(&payload);

    let resp = tokio::select! {
        r = request.send() => r.context("HTTP send failed")?,
        _ = cancel.cancelled() => return Err(CancelledError.into()),
    };

    let status = resp.status();
    if let (Some(slot), Some(l)) = (limits, crate::api::rate_limit::ApiLimits::from_headers(resp.headers(), crate::api::rate_limit::now_ms())) {
        *slot.lock().unwrap() = Some(l);
    }
    if status == 429 || status.as_u16() == 529 {
        // A typed error, so the retry loop can retry and honour retry-after.
        let retry_after = crate::api::rate_limit::retry_after(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_default();
        return Err(RateLimited { status: status.as_u16(), retry_after, detail }.into());
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("API error {}: {}", status, body);
    }

    let mut stream = resp.bytes_stream();
    let mut parser = SseParser::new();
    let mut handler = StreamHandler::new();

    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        for event in parser.feed(&bytes) {
                            if let Some(delta) = extract_block_delta(&event.data) {
                                on_delta(delta);
                            }
                            handler.on_event(&event.data);
                        }
                    }
                    Some(Err(e)) => anyhow::bail!("stream read error: {}", e),
                    None => break,
                }
            }
            _ = cancel.cancelled() => return Err(CancelledError.into()),
        }
    }

    if let Some(err) = handler.stream_error() {
        return Err(stream_error_to_anyhow(err));
    }
    Ok(handler.build_result())
}

/// Map an `error` event received mid-stream to an error. `overloaded_error`
/// and `rate_limit_error` become retryable `rate_limit:` errors, like an HTTP
/// 529 / 429 before the stream started; anything else is surfaced as-is
/// instead of being returned as a silently truncated turn.
fn stream_error_to_anyhow(err: &crate::api::sse::StreamError) -> anyhow::Error {
    match err.kind.as_str() {
        "overloaded_error" => {
            RateLimited { status: 529, retry_after: None, detail: format!("overloaded mid-stream: {}", err.message) }.into()
        }
        "rate_limit_error" => RateLimited { status: 429, retry_after: None, detail: format!("mid-stream: {}", err.message) }.into(),
        _ => anyhow::anyhow!("API stream error {}: {}", err.kind, err.message),
    }
}

// ---------------------------------------------------------------------------
// Retry wrapper
// ---------------------------------------------------------------------------

fn is_rate_limit_error(e: &anyhow::Error) -> bool {
    e.to_string().starts_with("rate_limit:")
}

fn backoff_delay(attempt: u32) -> std::time::Duration {
    let ms = (BASE_DELAY_MS * (1u64 << attempt.min(5))).min(MAX_BACKOFF_MS);
    std::time::Duration::from_millis(ms)
}

/// How long to wait before a retry: what the API asked for, else backoff.
fn retry_delay(e: &anyhow::Error, attempt: u32) -> std::time::Duration {
    e.downcast_ref::<RateLimited>()
        .and_then(|r| r.retry_after)
        .map(|d| d.min(MAX_WAIT))
        .unwrap_or_else(|| backoff_delay(attempt))
}

/// Output tokens a request is assumed to need from the output bucket.
const OUTPUT_NEED: u64 = 4096;

/// How long to wait before the next request: until the API's buckets (as
/// the last response left them, refilled since) can hold about what the
/// last request used, and, with a `rate_limit` cap, until the window is
/// under 80% of it.
fn wait_before_request(config: &AppConfig, state: &crate::shared::SharedState) -> std::time::Duration {
    use crate::api::rate_limit as rl;
    let now = rl::now_ms();
    let need_in = (*state.last_request_input.lock().unwrap()).max(1000);
    let need_out = OUTPUT_NEED.min(config.max_tokens as u64);
    let api = state.api_limits.lock().unwrap().map_or(std::time::Duration::ZERO, |l| l.wait_for(need_in, need_out, now));
    let own = if config.rate_limit == 0 {
        std::time::Duration::ZERO
    } else {
        let mut calls = state.recent_calls.lock().unwrap();
        rl::prune_old_calls(&mut calls);
        if rl::window_tokens(&calls) > config.rate_limit * 8 / 10 {
            calls.first().map_or(std::time::Duration::ZERO, |oldest| {
                std::time::Duration::from_millis(rl::RATE_WINDOW_MS.saturating_sub(now.saturating_sub(oldest.ts)))
            })
        } else {
            std::time::Duration::ZERO
        }
    };
    api.min(MAX_WAIT).max(own)
}

// ---------------------------------------------------------------------------
// Shared-state retry wrapper (Phase 4)
// ---------------------------------------------------------------------------

/// Calls `stream_turn` with a pre-call token-budget throttle (sliding window
/// in `SharedState::recent_calls`) and retry on 429/529.
///
/// Critical invariant: the `recent_calls` mutex is released **before** any
/// `.await`.  The block-scope pattern below enforces this.
pub async fn call_with_retry_shared(
    client:    &reqwest::Client,
    config:    &AppConfig,
    state:     &std::sync::Arc<crate::shared::SharedState>,
    messages:  &[Message],
    system:    &[SystemBlock],
    cancel:    CancellationToken,
    notify_fn: impl Fn(&str),
    on_delta:  impl Fn(BlockDeltaEvent),
) -> anyhow::Result<TurnResult> {
    // Locks are taken and released inside; none is held across an await.
    let wait = wait_before_request(config, state);
    if wait >= std::time::Duration::from_millis(500) {
        let why = if config.rate_limit > 0 && state.api_limits.lock().unwrap().is_none() {
            "Syl's own rate_limit cap"
        } else {
            "the API's per-minute token limit"
        };
        tracing::info!(wait_ms = wait.as_millis(), "waiting for {why}");
        notify_fn(&format!("[Rate limit] Waiting {}s for {why}...", wait.as_secs().max(1)));
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = cancel.cancelled() => return Err(CancelledError.into()),
        }
    }

    let mut attempts = 0u32;
    loop {
        match stream_turn_observed(client, config, messages, system, cancel.clone(), &on_delta, Some(&state.api_limits)).await {
            Ok(result) => {
                let uncached = result.usage.input_tokens + result.usage.cache_write_tokens;
                *state.last_request_input.lock().unwrap() = uncached;
                {
                    let mut calls = state.recent_calls.lock().unwrap();
                    crate::api::rate_limit::record_rate_call(&mut calls, uncached);
                }
                tracing::debug!(
                    input_tokens  = result.usage.input_tokens,
                    output_tokens = result.usage.output_tokens,
                    "turn complete"
                );
                return Ok(result);
            }
            Err(e) if is_rate_limit_error(&e) && attempts < MAX_RETRIES => {
                let delay = retry_delay(&e, attempts);
                tracing::warn!(attempt = attempts, delay_ms = delay.as_millis(), error = %e, "rate limited, retrying");
                if delay >= std::time::Duration::from_secs(2) {
                    let what = if e.to_string().starts_with("rate_limit:529") { "The API is overloaded" } else { "The API's rate limit was reached" };
                    notify_fn(&format!("[Rate limit] {what}; retrying in {}s...", delay.as_secs()));
                }
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => return Err(CancelledError.into()),
                }
                attempts += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool definitions for the Anthropic request
// ---------------------------------------------------------------------------

/// Return the tool definitions that Syl exposes to the model.
pub fn syl_tools() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "run_bash",
            "description": "Run a bash command inside the project container.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The bash command to run."}
                },
                "required": ["command"]
            }
        },
        {
            "name": "memory_read",
            "description": "Retrieve persistent memories from previous sessions. Call with no key to list all memories, or with a specific key to read one.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Topic name to read (e.g. 'project-stack'). Omit or leave empty to list all memories."}
                },
                "required": []
            }
        },
        {
            "name": "memory_write",
            "description": "Write or update a memory entry.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "key":     {"type": "string", "description": "Memory key."},
                    "content": {"type": "string", "description": "Content to store."}
                },
                "required": ["key", "content"]
            }
        },
        {
            "name": "workflow_write",
            "description": "Create a new workflow pipeline. Workflows are run with the /workflow slash command. Will refuse to overwrite an existing workflow — delete it first if you need to replace it.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name":    {"type": "string", "description": "Workflow name in kebab-case (e.g. 'ship', 'deploy-staging'). Must be unique."},
                    "content": {"type": "string", "description": "Workflow (recipe) content in markdown: a `# Title` heading, a description line, then steps as list items. A command step is `- Label: \\`command\\`` (append `(retry N)` to retry a failing command); an ask step is `- Label: ask: a prompt for Syl`, run as a turn. Optional front matter `---\\nparams: version, branch\\n---` declares parameters, used as {{version}} in steps and given as `/workflow name version=1.2`. Example:\n# Ship\nTest and push.\n\n- Run tests: `cargo test` (retry 1)\n- Push: `git push`"}
                },
                "required": ["name", "content"]
            }
        },
        {
            "name": "workflow_delete",
            "description": "Delete a workflow by name.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "read_file",
            "description": "Read lines of a file, numbered (100 lines by default; set start_line/end_line for other windows). Read-only; runs in parallel with other read-only calls.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path":       {"type": "string"},
                    "start_line": {"type": "integer"},
                    "end_line":   {"type": "integer"}
                },
                "required": ["path"]
            }
        },
        {
            "name": "edit_file",
            "description": "Edit a file by exact string replacement. old_string must match the file's current text exactly (including indentation and line breaks) and occur exactly once, unless replace_all is true. Include enough surrounding lines to make it unique. Returns a unified diff of the change. Use write_file to create a file or replace all of it.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path":        {"type": "string", "description": "File path, relative to /workspace or absolute."},
                    "old_string":  {"type": "string", "description": "Exact text to replace."},
                    "new_string":  {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring exactly one. Default false."}
                },
                "required": ["path", "old_string", "new_string"]
            }
        },
        {
            "name": "write_file",
            "description": "Create a file, or replace a file's whole contents. Parent directories are created. Returns a unified diff against the previous contents. Prefer edit_file for changes to part of an existing file.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path":    {"type": "string", "description": "File path, relative to /workspace or absolute."},
                    "content": {"type": "string", "description": "The complete new contents."}
                },
                "required": ["path", "content"]
            }
        },
        {
            "name": "grep",
            "description": "Search file contents with a regular expression (ripgrep syntax; respects .gitignore). Returns matching lines as path:line:text. Read-only; several calls in one response run in parallel.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern":     {"type": "string", "description": "Regular expression to search for."},
                    "path":        {"type": "string", "description": "File or directory to search. Default: the workspace."},
                    "glob":        {"type": "string", "description": "Only search files matching this glob, e.g. '*.rs' or 'src/**/*.ts'."},
                    "ignore_case": {"type": "boolean"},
                    "context":     {"type": "integer", "description": "Lines of context around each match (0-10). Default 0."},
                    "files_only":  {"type": "boolean", "description": "List matching file paths only."}
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "glob",
            "description": "List files whose paths match a glob, e.g. 'src/**/*.rs' or '*.md'. Hidden files are skipped unless the pattern names them. Read-only; runs in parallel with other read-only calls.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern; ** matches any number of directories."},
                    "path":    {"type": "string", "description": "Directory to search from. Default: the workspace."}
                },
                "required": ["pattern"]
            }
        },
        {
            "name": "ls",
            "description": "List a directory with sizes and permissions. Read-only; runs in parallel with other read-only calls.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to list. Default: the workspace."}
                },
                "required": []
            }
        },
        {
            "name": "dispatch_agent",
            "description": "Fetch a URL, process its content via a sub-agent, and return a structured result. Use for read-only remote tasks (research, summarisation, data extraction) to preserve your own context window. Not suitable for JavaScript-rendered pages.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "task":          {"type": "string", "enum": ["fetch", "summarize", "extract"], "description": "fetch: return relevant excerpt; summarize: return summary + key_points; extract: return structured data list."},
                    "source":        {"type": "string", "description": "URL to retrieve."},
                    "focus":         {"type": "string", "description": "Tightly scoped instruction describing exactly what to find or return."},
                    "output_format": {"type": "string", "enum": ["text", "json", "list"], "description": "How the result should be structured."},
                    "max_length":    {"type": "integer", "description": "Token budget for the sub-agent response (max_tokens). Clamped to 100–4096. Default 800."}
                },
                "required": ["task", "source", "focus", "output_format", "max_length"]
            }
        },
        {
            "name": "show_image",
            "description": "Display an image file in the terminal. The path must be a file that exists inside the container workspace (e.g. a file the model just created).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the image file, relative to /workspace inside the container."
                    },
                    "caption": {
                        "type": "string",
                        "description": "Optional caption shown below the image."
                    }
                },
                "required": ["path"]
            }
        }
    ])
}
