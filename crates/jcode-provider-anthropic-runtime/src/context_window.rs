//! Context-window resolution for the Anthropic runtime.
//!
//! The default `Provider::context_window` uses the cache-free lookup, so a live
//! catalog limit for a Claude id that the static classifier does not know yet
//! never reached the TUI meter or the compaction budget and silently fell back
//! to 200K. Resolving through the cache-aware lookup fixes that (see #578).

/// Resolve the context window for `model` on the Anthropic route.
pub(crate) fn resolve(model: &str) -> usize {
    jcode_base::provider::context_limit_for_model_with_provider(model, Some("claude"))
        .unwrap_or(jcode_provider_core::DEFAULT_CONTEXT_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_native_1m_model_resolves_to_one_million() {
        assert_eq!(resolve("claude-opus-5"), 1_000_000);
    }

    #[test]
    fn standard_model_keeps_two_hundred_k() {
        assert_eq!(resolve("claude-sonnet-4-5"), 200_000);
    }

    #[test]
    fn opus_4_8_resolves_to_one_million() {
        assert_eq!(resolve("claude-opus-4-8"), 1_000_000);
        // Dotted, dated, [1m]-suffixed, and mixed-case forms normalize to the
        // same verified generation.
        assert_eq!(resolve("claude-opus-4.8"), 1_000_000);
        assert_eq!(resolve("claude-opus-4-8[1m]"), 1_000_000);
        assert_eq!(resolve("Claude-Opus-4-8"), 1_000_000);
    }

    #[test]
    fn fable_5_resolves_to_one_million() {
        assert_eq!(resolve("claude-fable-5"), 1_000_000);
        assert_eq!(resolve("claude-fable-5[1m]"), 1_000_000);
    }

    #[test]
    fn verified_models_still_resolve_to_one_million_with_a_stale_200k_cache() {
        // Regression for live sessions misreporting claude-opus-4-8 /
        // claude-fable-5 as 200K-window models: the dynamic context-limit
        // cache is populated once at startup from API catalog data, so an
        // over-cautious catalog entry captured before these generations were
        // verified must not shrink the meter. The verified classification is
        // authoritative and wins before the cache is consulted.
        jcode_base::provider::populate_context_limits(
            [
                ("claude-opus-4-8".to_string(), 200_000),
                ("claude-fable-5".to_string(), 200_000),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(resolve("claude-opus-4-8"), 1_000_000);
        assert_eq!(resolve("claude-fable-5"), 1_000_000);
        // 200K-capped generations still resolve to their real window.
        assert_eq!(resolve("claude-sonnet-4-5"), 200_000);
    }
}
