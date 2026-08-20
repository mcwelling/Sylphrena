/// Dynamic model discovery.
///
/// Fetches the list of available Claude models from the backend API so the
/// `/model` picker always reflects what is actually accessible, without
/// requiring a code change when Anthropic releases new models.
///
/// # Anthropic
/// `GET https://api.anthropic.com/v1/models` — returns a paginated list with
/// `id` (the API identifier) and `display_name` (human label).
///
/// # Bedrock
/// `GET https://bedrock.{region}.amazonaws.com/foundation-models?byProvider=Anthropic&byOutputModality=TEXT`
/// — SigV4-signed GET; returns `modelId` and `modelName`.  The `modelId` is
/// already the full Bedrock ID (`anthropic.claude-opus-...`), so no separate
/// friendly→Bedrock translation is needed for models the user picks here.
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

/// A model returned by the backend API (or the static fallback).
#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    /// The ID to pass in API requests.
    /// Anthropic: friendly name or versioned ID (e.g. `"claude-opus-4-6-20250514"`).
    /// Bedrock:   full model ID     (e.g. `"anthropic.claude-opus-4-6-20250514-v1:0"`).
    pub id: String,
    /// Human-readable label for the picker UI (e.g. `"Claude Opus 4.6"`).
    pub display_name: String,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Fetch available models from the backend.  On any failure (missing
/// credentials, network error, unexpected response shape) returns the
/// compile-time static fallback list from `config::CLAUDE_MODELS`.
pub async fn fetch_models(http: &reqwest::Client, config: &AppConfig) -> Vec<DiscoveredModel> {
    let result = match config.backend.as_str() {
        "bedrock" => fetch_bedrock_models(http, config).await,
        _ => fetch_anthropic_models(http, config).await,
    };

    match result {
        Ok(models) if !models.is_empty() => {
            tracing::info!(count = models.len(), backend = %config.backend, "model list fetched");
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
// Bedrock
// ---------------------------------------------------------------------------

/// Fetches Anthropic text models from the Bedrock foundation-models endpoint.
/// Uses a separate signing function because the management-plane endpoint
/// (`bedrock.`) differs from the runtime endpoint (`bedrock-runtime.`) and
/// GET requests carry no body or content-type.
async fn fetch_bedrock_models(
    http: &reqwest::Client,
    config: &AppConfig,
) -> anyhow::Result<Vec<DiscoveredModel>> {
    if config.api_key.is_empty() || config.aws_secret_key.is_empty() {
        anyhow::bail!("AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY not set");
    }

    // Query parameters must be sorted alphabetically for SigV4 canonical form.
    let query = "byOutputModality=TEXT&byProvider=Anthropic";
    let path = "/foundation-models";
    let url = format!(
        "https://bedrock.{}.amazonaws.com{}?{}",
        config.aws_region, path, query
    );

    let headers = crate::bedrock::sign_bedrock_get_request(
        path,
        query,
        &config.aws_region,
        &config.api_key, // AWS_ACCESS_KEY_ID
        &config.aws_secret_key,
        config.aws_session_token.as_deref(),
    );

    let mut req = http.get(&url);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }

    let resp = req.send().await.context("GET /foundation-models")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("/foundation-models returned {status}: {body}");
    }

    let body: serde_json::Value = resp.json().await.context("parse foundation-models body")?;

    let models: Vec<DiscoveredModel> = body["modelSummaries"]
        .as_array()
        .context("missing 'modelSummaries' in foundation-models response")?
        .iter()
        .filter_map(|m| {
            let id = m["modelId"].as_str()?.to_string();
            // Only include models that support streaming (required for Syl's invoke call)
            if !m["responseStreamingSupported"].as_bool().unwrap_or(false) {
                return None;
            }
            let display_name = m["modelName"].as_str().unwrap_or(id.as_str()).to_string();
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
