/// Stateful stream handler: accumulates blocks as SSE events arrive.
/// Call `on_event` for each event data object, then `build_result` at the end.
pub struct StreamHandler {
    blocks: std::collections::BTreeMap<usize, BlockState>,
    stop_reason: Option<String>,
    input_tokens:       u64,
    output_tokens:      u64,
    cache_read_tokens:  u64,
    cache_write_tokens: u64,
}

#[derive(Debug)]
enum BlockState {
    Text { text: String },
    ToolUse { id: String, name: String, input_str: String },
    Thinking { thinking: String, signature: Option<String> },
    Unknown,
}

impl StreamHandler {
    pub fn new() -> Self {
        StreamHandler {
            blocks: std::collections::BTreeMap::new(),
            stop_reason: None,
            input_tokens:       0,
            output_tokens:      0,
            cache_read_tokens:  0,
            cache_write_tokens: 0,
        }
    }

    pub fn on_event(&mut self, data: &serde_json::Value) {
        let event_type = data["type"].as_str().unwrap_or("");
        match event_type {
            "message_start" => {
                if let Some(u) = data["message"]["usage"].as_object() {
                    self.input_tokens  = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    self.output_tokens = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    self.cache_read_tokens  = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    self.cache_write_tokens = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                }
            }
            "content_block_start" => {
                let idx = data["index"].as_u64().unwrap_or(0) as usize;
                let block = &data["content_block"];
                let state = match block["type"].as_str().unwrap_or("") {
                    "text"     => BlockState::Text { text: String::new() },
                    "tool_use" => BlockState::ToolUse {
                        id:        block["id"].as_str().unwrap_or("").to_string(),
                        name:      block["name"].as_str().unwrap_or("").to_string(),
                        input_str: String::new(),
                    },
                    "thinking" => BlockState::Thinking { thinking: String::new(), signature: None },
                    _          => BlockState::Unknown,
                };
                self.blocks.insert(idx, state);
            }
            "content_block_delta" => {
                let idx = data["index"].as_u64().unwrap_or(0) as usize;
                let delta = &data["delta"];
                if let Some(state) = self.blocks.get_mut(&idx) {
                    match (state, delta["type"].as_str().unwrap_or("")) {
                        (BlockState::Text { text }, "text_delta") => {
                            if let Some(t) = delta["text"].as_str() {
                                text.push_str(t);
                            }
                        }
                        (BlockState::ToolUse { input_str, .. }, "input_json_delta") => {
                            if let Some(p) = delta["partial_json"].as_str() {
                                input_str.push_str(p);
                            }
                        }
                        (BlockState::Thinking { thinking, .. }, "thinking_delta") => {
                            if let Some(t) = delta["thinking"].as_str() {
                                thinking.push_str(t);
                            }
                        }
                        (BlockState::Thinking { signature, .. }, "signature_delta") => {
                            *signature = delta["signature"].as_str().map(|s| s.to_string());
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(sr) = data["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(sr.to_string());
                }
                if let Some(u) = data["usage"].as_object() {
                    if let Some(v) = u.get("output_tokens").and_then(|v| v.as_u64()) {
                        self.output_tokens = v;
                    }
                }
            }
            _ => {}
        }
    }

    pub fn build_result(self) -> crate::api::types::TurnResult {
        use crate::api::types::{ContentBlock, StopReason, TurnResult, Usage};

        let content: Vec<ContentBlock> = self.blocks.into_values().filter_map(|state| {
            match state {
                BlockState::Text { text } => Some(ContentBlock::Text { text, cache_control: None }),
                BlockState::ToolUse { id, name, input_str } => {
                    let input = if input_str.is_empty() {
                        serde_json::Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(&input_str).unwrap_or(serde_json::Value::Null)
                    };
                    Some(ContentBlock::ToolUse { id, name, input })
                }
                BlockState::Thinking { thinking, signature } => {
                    Some(ContentBlock::Thinking { thinking, signature })
                }
                BlockState::Unknown => None,
            }
        }).collect();

        let stop_reason = self.stop_reason.as_deref()
            .map(StopReason::from)
            .unwrap_or(StopReason::EndTurn);

        TurnResult {
            content,
            stop_reason,
            usage: Usage {
                input_tokens:       self.input_tokens,
                output_tokens:      self.output_tokens,
                cache_read_tokens:  self.cache_read_tokens,
                cache_write_tokens: self.cache_write_tokens,
            },
        }
    }
}

impl Default for StreamHandler {
    fn default() -> Self {
        Self::new()
    }
}
