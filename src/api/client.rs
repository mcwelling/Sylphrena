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

const MAX_RETRIES: u32 = 4;
const BASE_DELAY_MS: u64 = 500;

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
        "tools": syl_tools(),
        "messages": messages,
    });

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
    if status == 429 || status.as_u16() == 529 {
        // Return a typed error so call_with_retry can detect and retry
        anyhow::bail!("rate_limit:{}", status.as_u16());
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
        "overloaded_error" => anyhow::anyhow!("rate_limit:529 (overloaded mid-stream: {})", err.message),
        "rate_limit_error" => anyhow::anyhow!("rate_limit:429 (mid-stream: {})", err.message),
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
    let ms = BASE_DELAY_MS * (1u64 << attempt.min(5));
    std::time::Duration::from_millis(ms)
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
    // Pre-call throttle: compute sleep duration without holding the lock.
    let sleep_dur = {
        let mut calls = state.recent_calls.lock().unwrap();
        crate::api::rate_limit::prune_old_calls(&mut calls);
        let used   = crate::api::rate_limit::window_tokens(&calls);
        let budget = config.rate_limit;
        if used > (budget as f64 * 0.8) as u64 {
            calls.first().map(|oldest| {
                let elapsed = crate::api::rate_limit::now_ms().saturating_sub(oldest.ts);
                let wait_ms = crate::api::rate_limit::RATE_WINDOW_MS.saturating_sub(elapsed);
                std::time::Duration::from_millis(wait_ms)
            })
        } else {
            None
        }
    }; // lock released
    if let Some(dur) = sleep_dur {
        if !dur.is_zero() {
            tracing::info!(wait_ms = dur.as_millis(), "token budget at 80% — cooling down");
            notify_fn(&format!("[Token budget] Cooling down for {}s...", dur.as_secs().max(1)));
            tokio::select! {
                _ = tokio::time::sleep(dur) => {}
                _ = cancel.cancelled() => return Err(CancelledError.into()),
            }
        }
    }

    let mut attempts = 0u32;
    loop {
        match stream_turn(client, config, messages, system, cancel.clone(), &on_delta).await {
            Ok(result) => {
                // Record call — brief lock, no await
                {
                    let mut calls = state.recent_calls.lock().unwrap();
                    crate::api::rate_limit::record_rate_call(&mut calls, result.usage.input_tokens);
                } // lock released
                tracing::debug!(
                    input_tokens  = result.usage.input_tokens,
                    output_tokens = result.usage.output_tokens,
                    "turn complete"
                );
                return Ok(result);
            }
            Err(e) if is_rate_limit_error(&e) && attempts < MAX_RETRIES => {
                let delay = backoff_delay(attempts);
                tracing::warn!(attempt = attempts, delay_ms = delay.as_millis(), "rate limited, retrying");
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
                    "content": {"type": "string", "description": "Workflow content in markdown. Format: a `# Title` heading, a description line, then steps as list items like `- Label: \\`command\\``. Example:\n# Ship\nTest and push.\n\n- Run tests: `bb test`\n- Push: `git push`"}
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
            "description": "Read a file from the project (paginated).",
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
