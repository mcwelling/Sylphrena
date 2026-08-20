/// SharedState — all runtime-shared mutable data, wrapped in Arc<> for clone-into-task.
///
/// All fields use std::sync::Mutex (not tokio::sync::Mutex) because no field
/// needs to be held across an .await.  The invariant is: lock → modify → unlock,
/// fully synchronous each time.
use std::io::Stdout;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::api::rate_limit::RateCall;
use crate::api::types::{Message, Usage};
use crate::container::ContainerStats;
use crate::tui::animation::AnimHandle;
use crate::tui::spinner::SpinnerHandle;

pub struct SharedState {
    /// Shared stdout handle.  All TUI code (animation, spinner, print_to_scroll) locks this.
    pub stdout: Arc<Mutex<Stdout>>,

    /// Full conversation history (user + assistant turns).
    pub messages: Mutex<Vec<Message>>,

    /// Cumulative token counts for this session (top bar).
    pub session_tokens: Mutex<Usage>,

    /// Sliding window of API calls for TPM budget enforcement.
    pub recent_calls: Mutex<Vec<RateCall>>,

    /// Most recent container CPU/memory snapshot (bottom bar).
    pub container_stats: Mutex<Option<ContainerStats>>,

    /// Live spinner task, if any.
    pub spinner: Mutex<Option<SpinnerHandle>>,

    /// Live thinking animation task, if any.
    pub anim: Mutex<Option<AnimHandle>>,

    /// Cancellation token for the currently running agent turn.
    /// None when idle; Some when a turn is in flight.
    pub current_cancel: Mutex<Option<CancellationToken>>,

    /// Active model for this session.  Initialised from `AppConfig::model` and
    /// updated by the `/model` selector.  `run_agent_turn` reads this each turn
    /// so model switches take effect immediately on the next user message.
    pub current_model: Mutex<String>,

    /// Models available for the `/model` picker.  Populated at startup by
    /// `api::models::fetch_models`; initialised to the static fallback list
    /// so the picker works even before the first API response arrives.
    pub available_models: Mutex<Vec<crate::api::models::DiscoveredModel>>,
}

impl SharedState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stdout: Arc::new(Mutex::new(std::io::stdout())),
            messages: Mutex::new(Vec::new()),
            session_tokens: Mutex::new(Usage::default()),
            recent_calls: Mutex::new(Vec::new()),
            container_stats: Mutex::new(None),
            spinner: Mutex::new(None),
            anim: Mutex::new(None),
            current_cancel: Mutex::new(None),
            current_model: Mutex::new(String::new()),
            available_models: Mutex::new(crate::api::models::static_fallback()),
        })
    }
}
