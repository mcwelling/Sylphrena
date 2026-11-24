//! Permission answers and the optional reviewer.
//!
//! The agent loop asks the permission policy about each tool call (see
//! `permissions`). An `ask` answer goes first to the reviewer, when one is
//! configured: a cheap model that may approve the call. If it doesn't
//! (it blocks, errs or times out: it fails closed), the user is asked.
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::api::client::SystemBlock;
use crate::api::types::{ContentBlock, Message};
use crate::config::{AppConfig, ReviewerConfig};
use crate::model::{ModelAdapter, ModelRequest};

/// How the user answered a `PermissionRequested`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAnswer {
    /// Allow this call.
    Once,
    /// Allow this call, and save a rule allowing the same subject from now
    /// on in this project.
    Always,
    Reject,
}

/// Calls waiting for an answer, by tool_use id.
pub type Pending =
    std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<PermissionAnswer>>>;

/// What the reviewer said.
#[derive(Debug, Clone, PartialEq)]
pub enum Review {
    Allow(String),
    Block(String),
    /// No verdict (error, timeout, unreadable answer): ask the user.
    Unsure(String),
}

const REVIEWER_SYSTEM: &str = "You review tool calls that an AI coding agent wants to make. \
The agent works in a sandboxed container on the user's project. A rule set by the user marks \
this call as one to check before it runs. Answer ALLOW if it is a reasonable, expected step \
toward what the user asked, with no effects beyond that (no deleting or overwriting work the \
user didn't ask to change, no publishing, no sending data elsewhere). Otherwise answer BLOCK. \
Reply with ALLOW or BLOCK, then one short sentence saying why.";

/// Ask the reviewer about a call. `task` is the user's latest request.
pub async fn review(
    model: &dyn ModelAdapter,
    base: &AppConfig,
    reviewer: &ReviewerConfig,
    task: &str,
    tool: &str,
    input: &serde_json::Value,
    cancel: CancellationToken,
) -> Review {
    let config = AppConfig {
        model: reviewer.model.clone(),
        max_tokens: 300,
        thinking_budget: None,
        tools: false,
        ..base.clone()
    };
    let input_text = serde_json::to_string_pretty(input).unwrap_or_default();
    // The reviewer must see the whole call: a cut one could hide its point.
    if input_text.len() > crate::util::MAX_TOOL_OUTPUT * 2 {
        return Review::Unsure(format!("the call is too long to review ({} chars)", input_text.len()));
    }
    let prompt = format!(
        "The user's request:\n{}\n\nThe tool call:\ntool: {tool}\ninput:\n{input_text}",
        crate::util::truncate_output(task)
    );
    let messages = vec![Message::user_text(prompt)];
    let system = vec![SystemBlock::text(REVIEWER_SYSTEM)];
    let req = ModelRequest { config: &config, messages: &messages, system: &system };
    let call = model.complete(req, cancel, &|_| {}, &|_| {});
    let turn = match tokio::time::timeout(Duration::from_secs(reviewer.timeout_secs), call).await {
        Err(_) => return Review::Unsure(format!("the reviewer took over {}s", reviewer.timeout_secs)),
        Ok(Err(e)) => return Review::Unsure(format!("the reviewer failed: {e}")),
        Ok(Ok(t)) => t,
    };
    let text: String = turn
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    parse_verdict(&text)
}

/// Read the verdict: the reviewer's first word, punctuation stripped, must
/// be exactly ALLOW or BLOCK. Anything else ("Allowing this would...") is
/// no verdict, which means asking the user.
pub fn parse_verdict(text: &str) -> Review {
    let t = text.trim();
    let end = t.find(char::is_whitespace).unwrap_or(t.len());
    let word: String = t[..end].chars().filter(|c| c.is_ascii_alphabetic()).collect();
    let reason = t[end..].trim_start_matches([':', '.', ',', '-', '*', ' ', '\n']).trim().to_string();
    // A reply naming both (echoing the instructions) decides nothing.
    let names_block = reason.to_ascii_uppercase().split(|c: char| !c.is_ascii_alphabetic()).any(|w| w == "BLOCK");
    match word.as_str() {
        "ALLOW" if names_block => Review::Unsure(format!("the reviewer's answer is unclear: {}", crate::session::one_line(text, 80))),
        "ALLOW" => Review::Allow(reason),
        "BLOCK" => Review::Block(reason),
        _ => Review::Unsure(format!("the reviewer gave no verdict: {}", crate::session::one_line(text, 80))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts() {
        assert_eq!(parse_verdict("ALLOW: routine test run"), Review::Allow("routine test run".into()));
        assert_eq!(parse_verdict("**BLOCK** - deletes the repo"), Review::Block("deletes the repo".into()));
        assert_eq!(parse_verdict("ALLOW."), Review::Allow("".into()));
        for fuzzy in ["Allowing this would publish code, so BLOCK.", "ALLOW or BLOCK: BLOCK", "Allowable? BLOCK", "allow"] {
            assert!(matches!(parse_verdict(fuzzy), Review::Unsure(_)), "{fuzzy}");
        }
        assert!(matches!(parse_verdict("I think this is fine"), Review::Unsure(_)));
        assert!(matches!(parse_verdict(""), Review::Unsure(_)));
    }
}
