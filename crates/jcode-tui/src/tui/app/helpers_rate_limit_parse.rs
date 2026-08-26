//! Rate-limit error parsing (reset/retry timing) for TUI auto-retry logic.
use std::time::Duration;

use super::parse_clock_time_to_duration;

/// Whether an error string looks like an Anthropic rate limit / usage cap
/// (HTTP 429), independent of whether a reset time can be parsed out of it.
pub(crate) fn error_looks_like_rate_limit(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("hit your limit")
}

/// Parse rate limit reset time from error message
/// Returns the Duration until rate limit resets, if this is a rate limit error
pub(crate) fn parse_rate_limit_error(error: &str) -> Option<Duration> {
    let error_lower = error.to_lowercase();

    if !error_lower.contains("rate limit")
        && !error_lower.contains("rate_limit")
        && !error_lower.contains("429")
        && !error_lower.contains("too many requests")
        && !error_lower.contains("hit your limit")
    {
        return None;
    }

    if let Some(idx) = error_lower.find("retry") {
        let after = &error_lower[idx..];
        for word in after.split_whitespace() {
            if let Some(secs) = pure_digit_seconds(word)
                && secs > 0
                && secs < 86400
            {
                return Some(Duration::from_secs(secs));
            }
        }
    }

    if let Some(idx) = error_lower.find("resets") {
        let after = &error_lower[idx..];
        for word in after.split_whitespace() {
            let word = word.trim_matches(|c: char| c == '·' || c == ' ');
            if (word.ends_with("am") || word.ends_with("pm"))
                && let Some(duration) = parse_clock_time_to_duration(word)
            {
                return Some(duration);
            }
        }
    }

    if let Some(idx) = error_lower.find("reset") {
        let after = &error_lower[idx..];
        // Unit-suffixed durations like "resets in 30d 4h 29m" (OpenAI usage
        // limit messages). Without this, "30d" would parse as 30 seconds and
        // schedule a bogus 30s auto-retry against a limit that resets in days.
        let mut unit_total = Duration::ZERO;
        let mut saw_unit = false;
        for word in after.split_whitespace().take(8) {
            let digits: String = word.chars().take_while(|c| c.is_ascii_digit()).collect();
            let rest = &word[digits.len()..];
            if digits.is_empty() {
                continue;
            }
            let value: u64 = match digits.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let secs = match rest.trim_matches(|c: char| !c.is_ascii_alphabetic()) {
                "d" => Some(value * 86400),
                "h" => Some(value * 3600),
                "m" | "min" => Some(value * 60),
                "s" | "sec" => Some(value),
                _ => None,
            };
            if let Some(secs) = secs {
                unit_total += Duration::from_secs(secs);
                saw_unit = true;
            }
        }
        if saw_unit {
            // Only auto-retry within a day; longer windows should be treated
            // as terminal by the caller (fallback offer / stop auto-poke).
            if unit_total > Duration::ZERO && unit_total < Duration::from_secs(86400) {
                return Some(unit_total);
            }
            return None;
        }
        for word in after.split_whitespace() {
            if let Some(secs) = pure_digit_seconds(word)
                && secs > 0
                && secs < 86400
            {
                return Some(Duration::from_secs(secs));
            }
        }
    }

    None
}

/// Seconds parsed from a token that is *entirely* digits after trimming
/// surrounding punctuation (quotes, commas, colons, brackets).
///
/// A rate-limit error may embed a `request_id` such as `req_3000` or
/// `req_011CePo4...`. The previous `trim_matches(!is_ascii_digit)` mined the
/// digits out of such ids and scheduled a bogus hold, so a `request_id` could
/// silently drive retry policy. Requiring a pure-digit core keeps legitimate
/// numeric hints like `30` (from "retry after 30 seconds") while rejecting any
/// token with interleaved or leading/trailing letters.
fn pure_digit_seconds(word: &str) -> Option<u64> {
    let core = word.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | ',' | '.' | ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}'
        )
    });
    if core.is_empty() || !core.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // Fold the (already validated all-digit) token with checked arithmetic. This
    // yields None on overflow via `?` without an ok-discarding call (which the
    // swallowed-error budget forbids) and without a manual Result-to-Option match
    // (which clippy's `manual_ok` forbids).
    let mut secs: u64 = 0;
    for byte in core.bytes() {
        secs = secs.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    }
    Some(secs)
}

#[cfg(test)]
mod rate_limit_parse_tests {
    use super::parse_rate_limit_error;
    use std::time::Duration;

    #[test]
    fn usage_limit_reset_in_days_does_not_schedule_bogus_short_retry() {
        // "30d" must not be misread as 30 seconds.
        let err = "Rate limited: The usage limit has been reached. Plan: team. \
                   Resets in 30d 4h 29m (2026-08-21 04:31 UTC).";
        assert_eq!(parse_rate_limit_error(err), None);
    }

    #[test]
    fn unit_suffixed_reset_within_a_day_is_parsed() {
        let err = "429 rate limit exceeded. Resets in 2h 5m.";
        assert_eq!(
            parse_rate_limit_error(err),
            Some(Duration::from_secs(2 * 3600 + 5 * 60))
        );
    }

    #[test]
    fn plain_retry_seconds_still_parse() {
        let err = "429 Too Many Requests: retry after 30 seconds";
        assert_eq!(parse_rate_limit_error(err), Some(Duration::from_secs(30)));
    }

    #[test]
    fn bare_anthropic_json_error_with_request_id_yields_no_duration() {
        // The dead-turn hazard: Anthropic's bare rate-limit error carries no
        // reset time, only a `request_id`. The parser must never mine a retry
        // duration out of the id (a `request_id` must not drive retry policy).
        // These are the exact strings the daemon surfaced (see scout report).
        let err = "Retryable stream error: {\"type\":\"error\",\"error\":{\"details\":null,\
                   \"type\":\"rate_limit_error\",\"message\":\"Rate limited\"},\
                   \"request_id\":\"req_011CePo4JJyJeBKdhQ9aGU87\"}";
        assert_eq!(parse_rate_limit_error(err), None);

        let wrapped = "Failed after 3 retries: Retryable stream error: {\"type\":\"error\",\
                       \"error\":{\"details\":null,\"type\":\"rate_limit_error\",\
                       \"message\":\"Rate limited\"},\
                       \"request_id\":\"req_011CePo5oo3J2YdKatBNRNeD\"}";
        assert_eq!(parse_rate_limit_error(wrapped), None);
    }

    #[test]
    fn request_id_with_contiguous_digits_does_not_drive_retry() {
        // A `request_id` whose token is a single contiguous digit run (e.g.
        // `req_3000`) previously got its digits mined by `trim_matches` and
        // scheduled a bogus 3000s hold. A request id must never yield a
        // duration, regardless of its digit shape.
        let err = "429 rate_limit_error; please retry. request_id req_3000";
        assert_eq!(parse_rate_limit_error(err), None);
    }
}
