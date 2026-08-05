pub(crate) fn anthropic_oauth_route_availability(model: &str) -> (bool, String) {
    if model.ends_with("[1m]") && !crate::usage::has_extra_usage() {
        (false, "requires extra usage".to_string())
    } else {
        (true, String::new())
    }
}

pub(crate) fn anthropic_api_key_route_availability(model: &str) -> (bool, String) {
    if model.ends_with("[1m]") && !crate::usage::has_extra_usage() {
        (false, "requires extra usage".to_string())
    } else {
        (true, String::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_route_available_regardless_of_subscription_tier() {
        // Pro-tier (and any other) OAuth accounts must not be client-side
        // blocked from Opus models; only Anthropic's own API is the
        // authoritative source of entitlement failures.
        let (available, reason) = anthropic_oauth_route_availability("claude-opus-4-8");
        assert!(available, "expected opus route to be available: {reason}");

        let (available, reason) = anthropic_oauth_route_availability("claude-opus-5");
        assert!(available, "expected opus route to be available: {reason}");
    }

    #[test]
    fn extra_usage_1m_gate_is_preserved() {
        let (available, reason) = anthropic_oauth_route_availability("claude-sonnet-5[1m]");
        // Without extra usage granted, the [1m] suffix must still be gated.
        if !crate::usage::has_extra_usage() {
            assert!(!available);
            assert_eq!(reason, "requires extra usage");
        }
    }
}
