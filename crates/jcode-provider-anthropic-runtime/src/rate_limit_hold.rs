//! Rate-limit hold classification for terminal Anthropic errors.
//!
//! Extracted from `lib.rs` so the oversized runtime module does not keep
//! growing. These helpers decide whether a *terminal* provider error (the retry
//! loop has given up) is a rate limit, and if so what retry-after hint to carry
//! back to the client so it can hold and reschedule the turn instead of dropping
//! it.

use anyhow::Result;
use jcode_message_types::StreamEvent;
use tokio::sync::mpsc;

/// Floor hold for a burst rate limit that arrives without a `Retry-After`
/// header (Anthropic's bare `{"type":"rate_limit_error","message":"Rate
/// limited"}`). Anthropic 429s are frequently burst/concurrency throttling
/// rather than window exhaustion, so a short bounded wait lets the throttle
/// clear instead of thrashing. Bounded by the same 60s cap the retry-after
/// core enforces.
pub(crate) const RATE_LIMIT_BURST_FLOOR_SECS: u64 = 60;

/// Whether an error string is specifically an Anthropic rate limit (HTTP 429),
/// as opposed to a generic transient/server error. Used to trigger a reactive
/// account switch: only a rate limit means "this account is capped, try
/// another", where a 5xx or transport blip is not account-specific.
pub(crate) fn is_rate_limit_error(error_str: &str) -> bool {
    let lower = error_str.to_ascii_lowercase();
    lower.contains("429 too many requests")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
}

/// Retry-after hint (in whole seconds) to attach to a *terminal* rate-limit
/// error so the client can hold the turn and reschedule it instead of dropping
/// it. Prefers the last-seen `Retry-After` header carried through the error
/// chain; falls back to a bounded burst floor for a bare "Rate limited" 429
/// with no header. Returns `None` for any non-rate-limit error so ordinary
/// failures keep their existing (no-retry) semantics.
pub(crate) fn rate_limit_retry_after_secs(error: &anyhow::Error) -> Option<u64> {
    let error_str = format!("{error:#}").to_lowercase();
    if !is_rate_limit_error(&error_str) {
        return None;
    }
    let header_secs = jcode_provider_core::retry_after::retry_after_from_error(error)
        .map(|delay| delay.as_secs())
        // A header that resolved to 0 remaining (already elapsed) is not a
        // useful hold: fall back to the burst floor so the client still waits.
        .filter(|secs| *secs > 0);
    Some(header_secs.unwrap_or(RATE_LIMIT_BURST_FLOOR_SECS))
}

/// Emit a terminal provider error to the stream, preserving the retry-after
/// hint when it is a rate limit.
///
/// A rate limit is sent as a structured `StreamEvent::Error` carrying the hint
/// (so the client holds and reschedules the turn instead of dropping it, since a
/// bare `anyhow` error would arrive with `retry_after_secs = None`). Any other
/// error keeps its existing no-retry semantics via `Err(error)`.
pub(crate) async fn emit_terminal_error(
    tx: &mpsc::Sender<Result<StreamEvent>>,
    error: anyhow::Error,
) {
    if let Some(retry_after_secs) = rate_limit_retry_after_secs(&error) {
        let _ = tx
            .send(Ok(StreamEvent::Error {
                message: format!("{error:#}"),
                retry_after_secs: Some(retry_after_secs),
            }))
            .await;
    } else {
        let _ = tx.send(Err(error)).await;
    }
}

/// Emit the "retry budget exhausted" failure, preserving a rate-limit
/// retry-after hint.
///
/// The hint is read from the ORIGINAL error before its source chain (a possible
/// `Retry-After` header) is flattened into the wrapped message string.
pub(crate) async fn emit_exhausted_error(
    tx: &mpsc::Sender<Result<StreamEvent>>,
    max_retries: u32,
    error: anyhow::Error,
) {
    let retry_after_secs = rate_limit_retry_after_secs(&error);
    let message = format!("Failed after {} retries: {:#}", max_retries, error);
    if let Some(retry_after_secs) = retry_after_secs {
        let _ = tx
            .send(Ok(StreamEvent::Error {
                message,
                retry_after_secs: Some(retry_after_secs),
            }))
            .await;
    } else {
        let _ = tx.send(Err(anyhow::Error::msg(message))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{RATE_LIMIT_BURST_FLOOR_SECS, rate_limit_retry_after_secs};
    use crate::MAX_RETRIES;

    /// A terminal rate-limit error must carry a `retry_after_secs` hint so the
    /// client can hold and reschedule the turn instead of dropping it. A bare
    /// Anthropic "Rate limited" 429 with no `Retry-After` header falls back to the
    /// bounded burst floor.
    #[test]
    fn terminal_rate_limit_without_header_uses_burst_floor() {
        let bare = anyhow::anyhow!(
            "Retryable stream error: {{\"type\":\"error\",\"error\":{{\"type\":\"rate_limit_error\",\"message\":\"Rate limited\"}},\"request_id\":\"req_011CePo4JJyJeBKdhQ9aGU87\"}}"
        );
        assert_eq!(
            rate_limit_retry_after_secs(&bare),
            Some(RATE_LIMIT_BURST_FLOOR_SECS),
            "a bare rate-limit error must still yield a bounded retry-after hint"
        );

        let wrapped = anyhow::anyhow!("Failed after {} retries: {:#}", MAX_RETRIES, bare);
        assert_eq!(
            rate_limit_retry_after_secs(&wrapped),
            Some(RATE_LIMIT_BURST_FLOOR_SECS),
            "the exhausted-loop message must also yield a retry-after hint"
        );
    }

    /// When the 429 carried a `Retry-After` header, that value (capped by the
    /// retry-after core) is preferred over the burst floor.
    #[test]
    fn terminal_rate_limit_prefers_retry_after_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("7"),
        );
        let retry_after = jcode_provider_core::retry_after::retry_after(&headers);
        let error = jcode_provider_core::retry_after::error_with_retry_after(
            "Anthropic API error (429 Too Many Requests): rate_limit_error".to_string(),
            retry_after,
        );

        let secs =
            rate_limit_retry_after_secs(&error).expect("a 429 with a header must yield a hint");
        assert!(
            secs > 0 && secs <= 7,
            "expected the header-derived hint (<=7s), got {secs}"
        );
        assert_ne!(
            secs, RATE_LIMIT_BURST_FLOOR_SECS,
            "a real header must be preferred over the burst floor"
        );
    }

    /// A non-rate-limit error must NOT be tagged with a retry-after hint, so
    /// ordinary failures keep their existing no-retry semantics.
    #[test]
    fn non_rate_limit_error_has_no_retry_after_hint() {
        let error = anyhow::anyhow!("500 internal server error: something broke");
        assert_eq!(rate_limit_retry_after_secs(&error), None);

        let timeout = anyhow::anyhow!("Stream read timeout: no data received for 120 seconds");
        assert_eq!(rate_limit_retry_after_secs(&timeout), None);
    }
}
