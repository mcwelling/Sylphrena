/// Dynamic model discovery.
///
/// Fetches the list of available Claude models from the Anthropic API so the
/// `/model` picker always reflects what is actually accessible, without
/// requiring a code change when Anthropic releases new models.
///
/// # Anthropic
/// `GET https://api.anthropic.com/v1/models` — returns a paginated list with
/// `id` (the API identifier) and `display_name` (human label).
///
/// # Fallback
/// On any failure (no credentials, network error, timeout) the function
/// returns the static `CLAUDE_MODELS` list from `config.rs` so the picker
/// always has something to show.
use anyhow::Context;

use crate::config::{AppConfig, ANTHROPIC_VERSION, CLAUDE_MODELS};

// ---------------------------------------------------------------------------
// Public type
// ---------------------------------------------------------------------------

/// A model returned by the API (or the static fallback).
#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    /// The ID to pass in API requests: a friendly name or versioned ID
    /// (e.g. `"claude-opus-4-6-20250514"`).
    pub id: String,
    /// Human-readable label for the picker UI (e.g. `"Claude Opus 4.6"`).
    pub display_name: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Fetch available models from the API.  On any failure (missing
/// credentials, network error, unexpected response shape) returns the
/// compile-time static fallback list from `config::CLAUDE_MODELS`.
pub async fn fetch_models(http: &reqwest::Client, config: &AppConfig) -> Vec<DiscoveredModel> {
    match fetch_anthropic_models(http, config).await {
        Ok(models) if !models.is_empty() => {
            tracing::info!(count = models.len(), "model list fetched");
            models
        }
        Ok(_) => {
            tracing::warn!("model list was empty, using static fallback");
            static_fallback()
        }
        Err(e) => {
            tracing::warn!("model fetch failed: {e:#}; using static fallback");
            static_fallback()
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic
// ---------------------------------------------------------------------------

async fn fetch_anthropic_models(
    http: &reqwest::Client,
    config: &AppConfig,
) -> anyhow::Result<Vec<DiscoveredModel>> {
    if config.api_key.is_empty() {
        anyhow::bail!("ANTHROPIC_API_KEY not set");
    }

    let resp = http
        .get("https://api.anthropic.com/v1/models")
        .header("x-api-key", &config.api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .send()
        .await
        .context("GET /v1/models")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("/v1/models returned {status}: {body}");
    }

    let body: serde_json::Value = resp.json().await.context("parse /v1/models body")?;

    let models: Vec<DiscoveredModel> = body["data"]
        .as_array()
        .context("missing 'data' array in /v1/models response")?
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            // Skip non-Claude entries (embeddings, future non-Claude models)
            if !id.contains("claude") {
                return None;
            }
            let display_name = m["display_name"]
                .as_str()
                .unwrap_or(id.as_str())
                .to_string();
            Some(DiscoveredModel { id, display_name })
        })
        .collect();

    Ok(models)
}

// ---------------------------------------------------------------------------
// Static fallback
// ---------------------------------------------------------------------------

/// Returns the compile-time `CLAUDE_MODELS` list when the API is unavailable.
pub fn static_fallback() -> Vec<DiscoveredModel> {
    CLAUDE_MODELS
        .iter()
        .map(|m| DiscoveredModel {
            id: m.id.to_string(),
            display_name: m.id.to_string(),
        })
        .collect()
}
