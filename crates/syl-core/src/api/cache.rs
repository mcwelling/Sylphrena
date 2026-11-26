//! Prompt-cache breakpoints for the `messages` of a request.
//!
//! The system prompt carries one breakpoint (tools and system together).
//! Here the last block of each of the last two user messages gets one:
//!
//! - the last message's, so this request writes everything it sends, the
//!   final tool results included, and the next request reads all of it
//!   back and is charged only for what it adds;
//! - the one before, which the previous request wrote, as a read point
//!   that is always within the API's 20-position lookback.
//!
//! That is three of the API's four. Tool results, images and text can carry
//! a breakpoint; thinking blocks can't, and neither can empty text. Before,
//! only the text of the last two assistant replies was tagged, so the final
//! message (often a large tool result) was never cached, and a run of tool
//! calls with no text in between didn't move the cache forward at all.
use crate::api::types::{Message, MessageContent};

/// How many user messages, counted from the end, get a breakpoint.
const TAGGED_USER_MESSAGES: usize = 2;

/// `messages` as the request's JSON, with the breakpoints set. A message
/// whose content is a plain string is always sent as one text block, so a
/// message's bytes are the same whether or not it is tagged (a prefix that
/// changed shape would miss the cache).
pub fn messages_with_cache_points(messages: &[Message]) -> serde_json::Value {
    let mut out: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let content = match &m.content {
                MessageContent::Text(t) => serde_json::json!([{ "type": "text", "text": t }]),
                MessageContent::Blocks(b) => serde_json::to_value(b).unwrap_or_else(|_| serde_json::json!([])),
            };
            serde_json::json!({ "role": m.role, "content": content })
        })
        .collect();
    let users: Vec<usize> = out
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "user")
        .map(|(i, _)| i)
        .rev()
        .take(TAGGED_USER_MESSAGES)
        .collect();
    for i in users {
        if let Some(blocks) = out[i]["content"].as_array_mut() {
            if let Some(b) = blocks.iter_mut().rev().find(|b| can_carry_breakpoint(b)) {
                b["cache_control"] = serde_json::json!({ "type": "ephemeral" });
            }
        }
    }
    serde_json::Value::Array(out)
}

fn can_carry_breakpoint(block: &serde_json::Value) -> bool {
    match block["type"].as_str() {
        Some("text") => block["text"].as_str().is_some_and(|t| !t.is_empty()),
        Some("tool_result" | "image" | "tool_use" | "document") => true,
        _ => false,
    }
}
