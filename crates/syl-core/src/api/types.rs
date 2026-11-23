use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub kind: String,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        CacheControl { kind: "ephemeral".to_string() }
    }
}

/// A content block in a message. Internally tagged by "type".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Encrypted thinking. Must be sent back unchanged with the rest of the
    /// assistant turn, like a `thinking` block.
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

impl ContentBlock {
    /// `thinking` or `redacted_thinking`.
    pub fn is_thinking(&self) -> bool {
        matches!(self, ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. })
    }
}

/// Message content: either a plain string or a block array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A single conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: MessageContent,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message { role: "user".to_string(), content: MessageContent::Text(text.into()) }
    }

    pub fn assistant_blocks(blocks: Vec<ContentBlock>) -> Self {
        Message { role: "assistant".to_string(), content: MessageContent::Blocks(blocks) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    /// The context window filled up mid-response (Claude 4.5 and later).
    ContextWindowExceeded,
    Other(String),
}

impl From<&str> for StopReason {
    fn from(s: &str) -> Self {
        match s {
            "end_turn"  => StopReason::EndTurn,
            "tool_use"  => StopReason::ToolUse,
            "max_tokens"=> StopReason::MaxTokens,
            "model_context_window_exceeded" => StopReason::ContextWindowExceeded,
            other       => StopReason::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input_tokens:       u64,
    pub output_tokens:      u64,
    pub cache_read_tokens:  u64,   // cache_read_input_tokens
    pub cache_write_tokens: u64,   // cache_creation_input_tokens
}

/// Result of a single streaming turn (replaces the old Text/ToolUse enum).
/// A turn can produce both text blocks and tool_use blocks; the caller
/// branches on `stop_reason`, not on a variant.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl TurnResult {
    pub fn text(&self) -> Option<&str> {
        self.content.iter().find_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
    }

    pub fn tool_uses(&self) -> Vec<&ContentBlock> {
        self.content.iter().filter(|b| matches!(b, ContentBlock::ToolUse { .. })).collect()
    }
}
