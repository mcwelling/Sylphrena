
use tokio_util::sync::CancellationToken;

use crate::api::client::SystemBlock;
use crate::api::types::{ContentBlock, Message, MessageContent, StopReason};
use crate::config::AppConfig;
use crate::model::{ModelAdapter, ModelRequest};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Summaries are written by the current Haiku: cheap, with a 1M window.
pub const COMPACT_MODEL: &str = "claude-haiku-5-5";
/// Default share of the context window kept free: compaction starts once
/// the context is within this fraction of the window.
pub const DEFAULT_COMPACT_BUFFER: f64 = 0.15;
/// Default size of the recent tail kept verbatim, in estimated tokens.
pub const DEFAULT_KEEP_TOKENS: u64 = 40_000;
/// Each tool result is cut to this many characters in the text the
/// summarizer reads; it needs the gist, not the full output.
const SUMMARY_BLOCK_CHARS: usize = 2_000;
/// The summarizer reads at most this many characters per request; longer
/// histories are summarized in chunks, each folding in the summary so far.
const SUMMARY_CHUNK_CHARS: usize = 600_000;

// ---------------------------------------------------------------------------
// Context size
// ---------------------------------------------------------------------------

/// The context window of `model` in tokens, when nothing better is known
/// (the Models API's `max_input_tokens`, or `context_window` in config).
/// Unknown models get the smaller window, so they compact early, not late.
pub fn default_context_window(model: &str) -> u64 {
    let m = model.to_ascii_lowercase();
    let million = ["-4-6", "-4-7", "-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5", "mythos-5"];
    if million.iter().any(|f| m.contains(f)) {
        1_000_000
    } else {
        200_000
    }
}

/// A rough token count for a message: about three bytes of JSON per token.
/// Only used for the parts of the context the API hasn't counted yet.
pub fn estimate_tokens(msg: &Message) -> u64 {
    serde_json::to_string(msg).map_or(0, |s| s.len() as u64 / 3) + 4
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    text.len() as u64 / 3
}

/// The context size at which compaction starts: the window less the
/// buffer (a fraction of it, clamped to 0..=0.5).
pub fn compaction_trigger(window: u64, buffer: f64) -> u64 {
    let free = (window as f64 * buffer.clamp(0.0, 0.5)) as u64;
    window.saturating_sub(free)
}

/// Whether a request of `tokens` (input plus room for the output) should
/// compact first.
pub fn needs_compaction(tokens: u64, window: u64, buffer: f64) -> bool {
    tokens >= compaction_trigger(window, buffer)
}

/// The recent tail to keep: what was asked for, but never more than half
/// the trigger, or the compacted context would trigger again at once.
pub fn effective_keep_tokens(keep: u64, window: u64, buffer: f64) -> u64 {
    keep.min(compaction_trigger(window, buffer) / 2)
}

/// The window size a "prompt is too long" error reports, if it says.
/// The API's message reads `prompt is too long: 1000123 tokens > 1000000 maximum`.
pub fn overflow_limit(error: &str) -> Option<u64> {
    let after = error.split('>').nth(1)?;
    after.split_whitespace().next()?.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok()
}

/// Whether an API error means the request didn't fit the context window.
pub fn is_context_overflow(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    e.contains("prompt is too long") || e.contains("exceed context limit")
}

// ---------------------------------------------------------------------------
// History helpers
// ---------------------------------------------------------------------------

/// The content to store in history for an assistant turn that stopped with
/// `stop`. Only a `tool_use` stop gets its tools run and answered; any
/// `tool_use` block in another turn (cut off by `max_tokens`, say) would be
/// left without a `tool_result`, which fails every later request, so it is
/// dropped. A turn left empty gets a placeholder text block.
pub fn drop_unanswered_tool_uses(content: &[ContentBlock], stop: &StopReason) -> Vec<ContentBlock> {
    if *stop == StopReason::ToolUse {
        return content.to_vec();
    }
    let mut kept: Vec<ContentBlock> = content
        .iter()
        .filter(|b| !matches!(b, ContentBlock::ToolUse { .. }))
        .cloned()
        .collect();
    if !kept.iter().any(|b| !b.is_thinking()) {
        kept.push(ContentBlock::Text { text: "(no response)".to_string(), cache_control: None });
    }
    kept
}

/// Remove every `thinking` and `redacted_thinking` block from `msgs`.
///
/// Thinking blocks are signed against everything that came before them. After
/// history is rewritten (compaction, or a resumed session under a new system
/// prompt) none of the old blocks would pass the prefix check on models with
/// preserved thinking, so all of them are dropped: with no old thinking sent
/// back there is nothing to fail, and new turns produce fresh blocks. An
/// assistant message left empty gets a placeholder text block so roles keep
/// alternating.
pub fn strip_thinking(msgs: Vec<Message>) -> Vec<Message> {
    msgs.into_iter()
        .map(|m| match m.content {
            MessageContent::Blocks(blocks) if m.role == "assistant" => {
                let mut kept: Vec<ContentBlock> = blocks
                    .into_iter()
                    .filter(|b| !b.is_thinking())
                    .collect();
                if kept.is_empty() {
                    kept.push(ContentBlock::Text {
                        text: "(continued)".to_string(),
                        cache_control: None,
                    });
                }
                Message { role: m.role, content: MessageContent::Blocks(kept) }
            }
            content => Message { role: m.role, content },
        })
        .collect()
}

/// Build compacted message history from a summary + kept messages.
/// None summary → return kept messages unchanged.
pub fn build_compacted_messages(summary: Option<&str>, keep_msgs: Vec<Message>) -> Vec<Message> {
    match summary {
        None => keep_msgs,
        Some(s) => {
            let mut result = vec![
                Message {
                    role:    "user".to_string(),
                    content: MessageContent::Text(
                        format!("[Context summary of earlier conversation]\n{}", s)
                    ),
                },
                Message {
                    role:    "assistant".to_string(),
                    content: MessageContent::Blocks(vec![ContentBlock::Text {
                        text: "Understood. I have the context from our earlier conversation and will continue from here.".to_string(),
                        cache_control: None,
                    }]),
                },
            ];
            result.extend(keep_msgs);
            result
        }
    }
}

// ---------------------------------------------------------------------------
// Compaction config
// ---------------------------------------------------------------------------

fn compaction_config(base: &AppConfig) -> AppConfig {
    AppConfig {
        model:           COMPACT_MODEL.to_string(),
        // Haiku 5.5 always thinks, and thinking spends output tokens, so
        // leave room for the summary after it.
        max_tokens:      16_384,
        // The summary must come back as text, not a tool call.
        tools:           false,
        thinking_budget: None,
        // Flags that should never be set for a sub-call
        resume:          false,
        fresh:           false,
        no_banner:       false,
        // All other fields (auth, rate-limit, UI, etc.)
        // pass through from base.  Using ..base.clone() means new
        // AppConfig fields are automatically included — no exhaustive
        // listing required.
        ..base.clone()
    }
}

// ---------------------------------------------------------------------------
// Message serialisation for summarisation prompt
// ---------------------------------------------------------------------------

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… [{} more chars]", &text[..cut], text.len() - cut)
}

fn message_to_text(m: &Message) -> String {
    let content = match &m.content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                ContentBlock::ToolUse { name, input, .. } => {
                    Some(format!("[tool_use: {} {}]", name, clip(&input.to_string(), SUMMARY_BLOCK_CHARS)))
                }
                ContentBlock::ToolResult { content, .. } => {
                    Some(format!("[tool_result: {}]", clip(content, SUMMARY_BLOCK_CHARS)))
                }
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    format!("{}: {}", m.role, content)
}

/// The conversation as plain text, in chunks of at most
/// `SUMMARY_CHUNK_CHARS` (a single huge message gets a chunk of its own).
fn messages_to_chunks(msgs: &[Message]) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for m in msgs {
        let t = clip(&message_to_text(m), SUMMARY_CHUNK_CHARS);
        if !cur.is_empty() && cur.len() + t.len() + 2 > SUMMARY_CHUNK_CHARS {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(&t);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

// ---------------------------------------------------------------------------
// Async summarisation via Haiku
// ---------------------------------------------------------------------------

const SUMMARY_POINTS: &str = "Cover:\n\
     - What the user was working on\n\
     - Key decisions made\n\
     - Important findings or errors encountered\n\
     - Files, commands and identifiers that later work will need\n\
     - Current state of the work and what was about to happen next";

/// Summarise a slice of old messages. Returns None on failure (the caller
/// drops the messages without a summary) or on cancel.
pub async fn summarize_messages(
    model:  &dyn ModelAdapter,
    config: &AppConfig,
    msgs:   &[Message],
    cancel: CancellationToken,
) -> Option<String> {
    let compact_cfg = compaction_config(config);
    let system = vec![SystemBlock::text(
        "You are a conversation summarizer. Produce a concise summary in plain prose."
    )];

    let mut summary: Option<String> = None;
    for chunk in messages_to_chunks(msgs) {
        let prompt = match &summary {
            None => format!(
                "Summarize the following conversation concisely. {SUMMARY_POINTS}\n\nConversation:\n{chunk}"
            ),
            Some(prev) => format!(
                "Here is a summary of the start of a conversation, then the next part of it. \
                 Write one concise summary of everything so far. {SUMMARY_POINTS}\n\n\
                 Summary so far:\n{prev}\n\nNext part:\n{chunk}"
            ),
        };
        let summary_msgs = vec![Message::user_text(&prompt)];
        let req = ModelRequest { config: &compact_cfg, messages: &summary_msgs, system: &system };
        match model.complete(req, cancel.clone(), &|_| {}, &|_| {}).await {
            Ok(turn) if turn.stop_reason != StopReason::EndTurn => {
                tracing::warn!(stop = ?turn.stop_reason, "compaction summary did not finish");
                return None;
            }
            Ok(turn) => {
                let text: String = turn.content.iter().filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                }).collect::<Vec<_>>().join("");
                if text.is_empty() {
                    tracing::warn!("compaction summary came back empty");
                    return None;
                }
                summary = Some(text);
            }
            Err(e) => {
                tracing::warn!("compaction summarize failed: {e}");
                return None;
            }
        }
    }
    summary
}

// ---------------------------------------------------------------------------
// Choosing what to keep
// ---------------------------------------------------------------------------

/// Where the kept tail of the history starts, and how it must be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepBoundary {
    /// Tail starts at a user message with no tool results: use the usual
    /// `[user summary, assistant ack, ...tail]` framing.
    UserText(usize),
    /// Tail starts at an assistant message (mid tool chain, no plain user
    /// message in the window): frame as `[user summary, ...tail]` so roles
    /// still alternate and every tool_result keeps its matching tool_use.
    Assistant(usize),
}

impl KeepBoundary {
    pub fn index(self) -> usize {
        match self {
            KeepBoundary::UserText(i) | KeepBoundary::Assistant(i) => i,
        }
    }
}

fn is_user_text(m: &Message) -> bool {
    m.role == "user"
        && match &m.content {
            MessageContent::Text(_) => true,
            MessageContent::Blocks(b) => !b.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })),
        }
}

/// The index from which the last messages add up to at least `keep_tokens`
/// (estimated). 0 when the whole history is smaller.
pub fn keep_window_start(messages: &[Message], keep_tokens: u64) -> usize {
    let mut total = 0u64;
    for (i, m) in messages.iter().enumerate().rev() {
        total += estimate_tokens(m);
        if total >= keep_tokens {
            return i;
        }
    }
    0
}

fn boundary_at(messages: &[Message], i: usize) -> Option<KeepBoundary> {
    let m = &messages[i];
    if is_user_text(m) {
        Some(KeepBoundary::UserText(i))
    } else if m.role == "assistant" {
        Some(KeepBoundary::Assistant(i))
    } else {
        None
    }
}

/// Choose the index at which the kept tail begins: the first message at or
/// after `window_start` that a tail can start with (a plain user message, or
/// an assistant message, whose tool results follow it). A tool-results
/// message can't start a tail. If the window has no such message (one huge
/// tool result, say), searches back from it. Index 0 is never chosen: there
/// would be nothing to summarize.
pub fn choose_keep_boundary(messages: &[Message], window_start: usize) -> Option<KeepBoundary> {
    let start = window_start.max(1);
    if start >= messages.len() {
        return None;
    }
    (start..messages.len())
        .find_map(|i| boundary_at(messages, i))
        .or_else(|| (1..start).rev().find_map(|i| boundary_at(messages, i)))
}
