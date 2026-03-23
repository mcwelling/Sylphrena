use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::api::client::{call_with_retry_shared, SystemBlock};
use crate::api::types::{ContentBlock, Message, MessageContent};
use crate::config::AppConfig;
use crate::shared::SharedState;

// ---------------------------------------------------------------------------
// Constants (match syl.bb lines 134-135)
// ---------------------------------------------------------------------------

pub const MAX_HISTORY:  usize = 30;
pub const COMPACT_KEEP: usize = 10;
const COMPACT_MODEL:    &str  = "claude-haiku-4-5-20251001";

// ---------------------------------------------------------------------------
// Existing helpers (unchanged)
// ---------------------------------------------------------------------------

/// Drop leading messages until the first plain user text message.
/// Returns empty vec if none found.
pub fn find_valid_start(msgs: Vec<Message>) -> Vec<Message> {
    let pos = msgs.iter().position(|m| {
        m.role == "user" && matches!(m.content, MessageContent::Text(_))
    });
    match pos {
        Some(i) => msgs[i..].to_vec(),
        None    => vec![],
    }
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
        max_tokens:      2048,
        thinking_budget: None,
        // Flags that should never be set for a sub-call
        resume:          false,
        fresh:           false,
        no_banner:       false,
        // All other fields (auth, backend, rate-limit, UI, etc.)
        // pass through from base.  Using ..base.clone() means new
        // AppConfig fields are automatically included — no exhaustive
        // listing required.
        ..base.clone()
    }
}

// ---------------------------------------------------------------------------
// Message serialisation for summarisation prompt
// ---------------------------------------------------------------------------

fn messages_to_text(msgs: &[Message]) -> String {
    msgs.iter().map(|m| {
        let content = match &m.content {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Blocks(blocks) => {
                blocks.iter().filter_map(|b| match b {
                    ContentBlock::Text { text, .. }              => Some(text.clone()),
                    ContentBlock::ToolUse { name, input, .. }    => Some(format!("[tool_use: {} {}]", name, input)),
                    ContentBlock::ToolResult { content, .. }     => Some(format!("[tool_result: {}]", content)),
                    ContentBlock::Thinking { .. }                => None,
                }).collect::<Vec<_>>().join("\n")
            }
        };
        format!("{}: {}", m.role, content)
    }).collect::<Vec<_>>().join("\n\n")
}

// ---------------------------------------------------------------------------
// Async summarisation via Haiku
// ---------------------------------------------------------------------------

/// Summarise a slice of old messages. Returns None on failure (caller falls
/// back to keeping the messages as-is).
pub async fn summarize_messages(
    http:   &reqwest::Client,
    config: &AppConfig,
    state:  &Arc<SharedState>,
    msgs:   &[Message],
    cancel: CancellationToken,
) -> Option<String> {
    let compact_cfg = compaction_config(config);

    let prompt = format!(
        "Summarize the following conversation concisely. Cover:\n\
         - What the user was working on\n\
         - Key decisions made\n\
         - Important findings or errors encountered\n\
         - Current state of the work\n\n\
         Conversation:\n{}",
        messages_to_text(msgs)
    );

    let summary_msgs = vec![Message::user_text(&prompt)];
    let system = vec![SystemBlock::text(
        "You are a conversation summarizer. Produce a concise summary in plain prose."
    )];

    match call_with_retry_shared(http, &compact_cfg, state, &summary_msgs, &system, cancel, |_| {}, |_| {}).await {
        Ok(turn) => {
            let text: String = turn.content.iter().filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            }).collect::<Vec<_>>().join("");
            if text.is_empty() { None } else { Some(text) }
        }
        Err(e) => {
            tracing::warn!("compaction summarize failed: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Public compact_history entry point
// ---------------------------------------------------------------------------

/// Compact the conversation history when it exceeds MAX_HISTORY messages.
/// Summarises the oldest messages via Haiku and keeps the COMPACT_KEEP most
/// recent messages, preserving a valid user-first start.
pub async fn compact_history(
    messages: Vec<Message>,
    http:     &reqwest::Client,
    config:   &AppConfig,
    state:    &Arc<SharedState>,
    cancel:   CancellationToken,
) -> Vec<Message> {
    if messages.len() <= MAX_HISTORY {
        return messages;
    }

    let keep_start = messages.len() - COMPACT_KEEP;
    let old  = messages[..keep_start].to_vec();
    let keep = find_valid_start(messages[keep_start..].to_vec());

    tracing::info!(
        old = old.len(), keep = keep.len(),
        "compacting history"
    );

    let summary = summarize_messages(http, config, state, &old, cancel).await;
    if summary.is_none() {
        tracing::warn!("compaction summary failed — keeping recent messages only");
    }
    build_compacted_messages(summary.as_deref(), keep)
}
