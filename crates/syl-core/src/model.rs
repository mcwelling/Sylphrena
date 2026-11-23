//! The seam between the agent loop and a model provider.
//!
//! Only Anthropic is implemented. The loop, compaction and `dispatch_agent`
//! talk to a `ModelAdapter`, so another provider can be added later without
//! touching them.
use std::sync::Arc;

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::api::client::{call_with_retry_shared, BlockDeltaEvent, SystemBlock};
use crate::api::types::{Message, TurnResult};
use crate::config::AppConfig;
use crate::shared::SharedState;

/// One model call: the config carries the model id, limits and thinking.
#[derive(Clone, Copy)]
pub struct ModelRequest<'a> {
    pub config: &'a AppConfig,
    pub messages: &'a [Message],
    pub system: &'a [SystemBlock],
}

/// Status text a provider wants shown, such as a rate-limit cool-down.
pub type NoticeFn<'a> = &'a (dyn Fn(&str) + Send + Sync);
/// Streaming text, thinking and tool-input deltas.
pub type DeltaFn<'a> = &'a (dyn Fn(BlockDeltaEvent) + Send + Sync);

pub trait ModelAdapter: Send + Sync {
    /// Stream one model turn, retrying transient failures. Cancelling
    /// `cancel` returns `CancelledError`.
    fn complete<'a>(
        &'a self,
        req: ModelRequest<'a>,
        cancel: CancellationToken,
        on_notice: NoticeFn<'a>,
        on_delta: DeltaFn<'a>,
    ) -> BoxFuture<'a, anyhow::Result<TurnResult>>;
}

/// The Anthropic Messages API, with the shared TPM budget in `state`.
pub struct AnthropicAdapter {
    http: reqwest::Client,
    state: Arc<SharedState>,
}

impl AnthropicAdapter {
    pub fn new(http: reqwest::Client, state: Arc<SharedState>) -> Self {
        AnthropicAdapter { http, state }
    }
}

impl ModelAdapter for AnthropicAdapter {
    fn complete<'a>(
        &'a self,
        req: ModelRequest<'a>,
        cancel: CancellationToken,
        on_notice: NoticeFn<'a>,
        on_delta: DeltaFn<'a>,
    ) -> BoxFuture<'a, anyhow::Result<TurnResult>> {
        Box::pin(call_with_retry_shared(
            &self.http,
            req.config,
            &self.state,
            req.messages,
            req.system,
            cancel,
            on_notice,
            on_delta,
        ))
    }
}
