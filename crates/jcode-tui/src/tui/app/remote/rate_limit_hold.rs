//! Rate-limit hold-duration classification for the remote error handler.
//!
//! Extracted from `server_events.rs` so that oversized module does not keep
//! growing. Decides how long to hold a pending turn when the server reports a
//! rate-limit failure, so the turn is rescheduled instead of dropped.

use std::time::Duration;

use crate::tui::app::helpers::{error_looks_like_rate_limit, parse_rate_limit_error};

/// Bounded default hold applied to a rate-limit error that carries no usable
/// reset time (no `retry_after_secs`, no parseable reset in the message text).
/// Short enough that a transient burst throttle clears promptly, long enough to
/// avoid immediately re-hammering the limiter. This mirrors the provider-side
/// burst floor and keeps the turn alive instead of dropping it.
const RATE_LIMIT_DEFAULT_HOLD_SECS: u64 = 60;

/// How long to hold a pending turn for a server error, if it is a rate limit.
///
/// Classifies a rate limit FIRST, before any duration parsing: a rate-limit
/// failure must ALWAYS hold the pending turn and schedule a resend, never drop
/// it, even when no reset time can be recovered (a bare Anthropic "Rate limited"
/// 429 with no `Retry-After` header and only a `request_id`). Prefers the
/// provider's `retry_after_secs`, then any reset time parsed from the message
/// text, and finally a bounded default floor so the hold always fires. The
/// `request_id` can never drive this timer (see `parse_rate_limit_error`).
///
/// Returns `None` for a non-rate-limit error so ordinary failures keep their
/// existing (no-hold) semantics.
pub(super) fn rate_limit_hold_duration(
    message: &str,
    retry_after_secs: Option<u64>,
) -> Option<Duration> {
    retry_after_secs
        .map(Duration::from_secs)
        .or_else(|| parse_rate_limit_error(message))
        .or_else(|| {
            error_looks_like_rate_limit(message)
                .then(|| Duration::from_secs(RATE_LIMIT_DEFAULT_HOLD_SECS))
        })
}
