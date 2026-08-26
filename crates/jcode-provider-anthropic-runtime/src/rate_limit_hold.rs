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
