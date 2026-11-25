//! State shared between the agent loop and its clients.
//!
//! Every field uses `std::sync::Mutex` because no lock is held across an
//! `.await`: lock, read or modify, unlock.
use std::sync::{Arc, Mutex};

use crate::api::client::SystemBlock;
use crate::api::models::DiscoveredModel;
use crate::api::rate_limit::RateCall;
use crate::api::types::{Message, Usage};

pub struct SharedState {
    /// Conversation history (user and assistant turns).
    pub messages: Mutex<Vec<Message>>,

    /// Cumulative token counts for this session.
    pub session_tokens: Mutex<Usage>,

    /// Sliding window of API calls, for the optional `rate_limit` cap.
    pub recent_calls: Mutex<Vec<RateCall>>,

    /// The API's rate limits, from the last response's headers.
    pub api_limits: Mutex<Option<crate::api::rate_limit::ApiLimits>>,

    /// Uncached input tokens (input plus cache writes) of the last request:
    /// about what the next one will need from the input bucket.
    pub last_request_input: Mutex<u64>,

    /// Model for the next turn. Initialised from `AppConfig::model` and
    /// changed with `AgentCommand::SetModel`.
    pub current_model: Mutex<String>,

    /// Models offered by `/model`. Starts as the static fallback list and is
    /// replaced by `api::models::fetch_models` at startup.
    pub available_models: Mutex<Vec<DiscoveredModel>>,

    /// System prompt frozen for the current conversation. Built when the
    /// conversation starts (no assistant turn yet) and then reused unchanged,
    /// because models with preserved thinking (Opus 5.5 and other 5.x
    /// models) reject or drop thinking blocks once `system` changes under
    /// them. Also keeps the prompt cache warm.
    pub system_prompt: Mutex<Option<Vec<SystemBlock>>>,

    /// Tool definitions sent besides the built-in ones, frozen with the
    /// system prompt (MCP tools, the `skill` tool).
    pub extra_tools: Mutex<std::sync::Arc<Vec<serde_json::Value>>>,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            messages: Mutex::new(Vec::new()),
            session_tokens: Mutex::new(Usage::default()),
            recent_calls: Mutex::new(Vec::new()),
            api_limits: Mutex::new(None),
            last_request_input: Mutex::new(0),
            current_model: Mutex::new(String::new()),
            available_models: Mutex::new(crate::api::models::static_fallback()),
            system_prompt: Mutex::new(None),
            extra_tools: Mutex::new(Default::default()),
        })
    }
}
