/// Cost estimation for Anthropic API usage.
///
/// Prices as of 2026-03. The model-matching strings are intentionally broad
/// (e.g. `opus-4` matches both `claude-opus-4-6` and any future `claude-opus-4-x`)
/// so that new patch releases don't silently fall through to the fallback rate.
/// If Anthropic releases a new major version with different pricing, add a new arm.
use crate::api::types::Usage;

pub fn compute_cost_usd(usage: &Usage, model: &str) -> f64 {
    // Input/output prices per 1M tokens
    let (in_price, out_price): (f64, f64) = if model.contains("opus-4") {
        (5.0, 25.0)   // claude-opus-4-x
    } else if model.contains("sonnet-4") {
        (3.0, 15.0)   // claude-sonnet-4-x
    } else if model.contains("haiku-4") {
        (1.0, 5.0)    // claude-haiku-4-x
    } else {
        (3.0, 15.0)   // safe fallback (sonnet rates)
    };

    // Cache read  = 10% of base input price  (Anthropic: "cache_read_input_tokens")
    // Cache write = 25% of base input price  (Anthropic: "cache_creation_input_tokens")
    let cr_price = in_price * 0.10;
    let cw_price = in_price * 0.25;

    let m = 1_000_000.0_f64;
    usage.input_tokens       as f64 / m * in_price
        + usage.output_tokens      as f64 / m * out_price
        + usage.cache_read_tokens  as f64 / m * cr_price
        + usage.cache_write_tokens as f64 / m * cw_price
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::Usage;

    #[test]
    fn cost_opus_only_output() {
        let u = Usage { output_tokens: 1_000_000, ..Default::default() };
        let cost = compute_cost_usd(&u, "claude-opus-4-6");
        assert!((cost - 25.0).abs() < 1e-9, "expected $25.00, got {cost}");
    }

    #[test]
    fn cost_zero_usage() {
        let u = Usage::default();
        assert_eq!(compute_cost_usd(&u, "claude-opus-4-6"), 0.0);
    }

    #[test]
    fn cost_unknown_model_fallback() {
        // Unknown model falls back to sonnet rates; should not panic and should
        // return a positive value when there are tokens.
        let u = Usage { input_tokens: 1_000_000, ..Default::default() };
        let cost = compute_cost_usd(&u, "claude-unknown-99");
        assert!(cost > 0.0);
    }
}
