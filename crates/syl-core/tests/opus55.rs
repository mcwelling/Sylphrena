//! Tests for the Claude Opus 5.5 default: model catalog, thinking request
//! shape on adaptive models, and thinking-block stripping on history rewrites.
use serde_json::json;
use syl_core::api::client::thinking_param;
use syl_core::api::compaction::strip_thinking;
use syl_core::api::types::{ContentBlock, Message, MessageContent};
use syl_core::config::{supports_manual_thinking, AppConfig, CLAUDE_MODELS, DEFAULT_MODEL};

#[test]
fn default_model_is_opus_5_5_and_in_catalog() {
    assert_eq!(DEFAULT_MODEL, "claude-opus-5-5");
    assert_eq!(CLAUDE_MODELS[0].id, DEFAULT_MODEL, "default should lead the picker");
    assert_eq!(AppConfig::default().model, DEFAULT_MODEL);
}

#[test]
fn manual_thinking_support_by_model() {
    for m in ["claude-opus-4-6", "claude-sonnet-4-6", "claude-haiku-4-5", "claude-haiku-4-5-20251001"] {
        assert!(supports_manual_thinking(m), "{m} should support budget_tokens");
    }
    for m in [
        "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5", "claude-haiku-5-5",
        "claude-fable-5-2", "claude-opus-4-7", "claude-opus-4-8",
    ] {
        assert!(!supports_manual_thinking(m), "{m} is adaptive-only");
    }
}

fn cfg(model: &str, budget: Option<u64>) -> AppConfig {
    let mut c = AppConfig::default();
    c.model = model.to_string();
    c.thinking_budget = budget;
    c
}

#[test]
fn thinking_param_shapes() {
    assert_eq!(thinking_param(&cfg("claude-opus-5-5", None)), None);
    assert_eq!(
        thinking_param(&cfg("claude-opus-5-5", Some(8000))),
        Some(json!({"type": "adaptive", "display": "summarized"}))
    );
    assert_eq!(
        thinking_param(&cfg("claude-opus-4-6", Some(8000))),
        Some(json!({"type": "enabled", "budget_tokens": 8000}))
    );
}

#[test]
fn strip_thinking_removes_only_thinking_and_keeps_roles() {
    let msgs = vec![
        Message::user_text("hi"),
        Message::assistant_blocks(vec![
            ContentBlock::Thinking { thinking: String::new(), signature: Some("sig".into()) },
            ContentBlock::ToolUse { id: "t1".into(), name: "run_bash".into(), input: json!({}) },
        ]),
        Message {
            role: "user".into(),
            content: MessageContent::Blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                is_error: None,
            }]),
        },
        // An assistant turn holding only thinking must not become empty.
        Message::assistant_blocks(vec![ContentBlock::Thinking {
            thinking: "x".into(),
            signature: Some("s2".into()),
        }]),
    ];
    let out = strip_thinking(msgs.clone());
    assert_eq!(out.len(), msgs.len());
    for (a, b) in out.iter().zip(msgs.iter()) {
        assert_eq!(a.role, b.role);
    }
    let has_thinking = out.iter().any(|m| match &m.content {
        MessageContent::Blocks(bs) => bs.iter().any(|b| matches!(b, ContentBlock::Thinking { .. })),
        _ => false,
    });
    assert!(!has_thinking);
    // tool_use and tool_result survive untouched
    assert!(matches!(&out[1].content, MessageContent::Blocks(bs) if matches!(bs[0], ContentBlock::ToolUse { .. })));
    assert_eq!(out[2], msgs[2]);
    assert!(matches!(&out[3].content, MessageContent::Blocks(bs) if bs.len() == 1 && matches!(bs[0], ContentBlock::Text { .. })));
}

#[test]
fn max_tokens_adds_budget_only_for_manual_thinking() {
    use syl_core::api::client::{request_max_tokens, MIN_THINKING_BUDGET};
    let base = AppConfig::default().max_tokens as u64;
    assert_eq!(request_max_tokens(&cfg("claude-opus-5-5", None)), base);
    // Adaptive models ignore the budget, so it must not inflate max_tokens.
    assert_eq!(request_max_tokens(&cfg("claude-opus-5-5", Some(100_000))), base);
    assert_eq!(request_max_tokens(&cfg("claude-opus-4-6", Some(8000))), base + 8000);
    // Below the API minimum the budget is raised, in both fields.
    assert_eq!(request_max_tokens(&cfg("claude-opus-4-6", Some(500))), base + MIN_THINKING_BUDGET);
    assert_eq!(
        thinking_param(&cfg("claude-opus-4-6", Some(500))),
        Some(json!({"type": "enabled", "budget_tokens": MIN_THINKING_BUDGET}))
    );
}

#[test]
fn strip_thinking_also_removes_redacted_thinking() {
    let msgs = vec![
        Message::user_text("hi"),
        Message::assistant_blocks(vec![
            ContentBlock::RedactedThinking { data: "opaque".into() },
            ContentBlock::Text { text: "hello".into(), cache_control: None },
        ]),
    ];
    let out = strip_thinking(msgs);
    assert!(matches!(&out[1].content, MessageContent::Blocks(bs)
        if bs.len() == 1 && matches!(bs[0], ContentBlock::Text { .. })));
}
