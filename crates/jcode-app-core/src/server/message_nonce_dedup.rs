//! Per-connection idempotency for inbound user `Request::Message` submissions.
//!
//! On a busy shared server a client that sends a queued follow-up while a turn
//! is still running gets an `"Already processing a message"` rejection. The
//! client's anti-loss recovery (issue #391) re-queues the same content and
//! re-adopts a running-turn state, so a transient busy race can turn one logical
//! submission into several accepted, appended user turns - the duplicate-message
//! injection bug.
//!
//! The authoritative fix is server-side idempotency: the client stamps each
//! logical submission with a stable `submission_nonce` that is preserved across
//! every busy/disconnect re-send, and the server records recently-accepted
//! nonces per connection. A duplicate nonce is acknowledged without appending a
//! second identical user turn. This mirrors the existing swarm-spawn
//! `request_nonce` dedup pattern and makes the append idempotent regardless of
//! how the client retries.
//!
//! The tracker is intentionally tiny and bounded: a FIFO ring of the most
//! recent accepted nonces per connection. A submission without a nonce (an older
//! client) is never deduplicated, preserving backward-compatible behavior.

use std::collections::VecDeque;

/// Maximum number of recently-accepted submission nonces retained per
/// connection. A user cannot get more than a handful of distinct submissions
/// ahead of a running turn, so a small ring is sufficient to absorb any
/// realistic busy-retry storm while staying O(1) in memory.
const MAX_TRACKED_NONCES: usize = 64;

/// Bounded FIFO set of recently-accepted submission nonces for one connection.
///
/// Not thread-safe by itself: it is owned by the single `handle_client` task
/// loop that serializes every request for its connection, so no locking is
/// required.
#[derive(Debug, Default)]
pub(super) struct MessageNonceTracker {
    // Ordered oldest-first so eviction is a cheap `pop_front`. Membership checks
    // are linear over a <=64 element ring, which is negligible next to the model
    // turn a real message triggers.
    seen: VecDeque<String>,
}

impl MessageNonceTracker {
    pub(super) fn new() -> Self {
        Self {
            seen: VecDeque::new(),
        }
    }

    /// Returns true when this nonce was already accepted on this connection.
    ///
    /// A `None` nonce (older client that does not stamp submissions) is never a
    /// duplicate: without a stable identity the server cannot safely dedup, so
    /// it keeps the pre-dedup behavior.
    pub(super) fn is_duplicate(&self, nonce: Option<&str>) -> bool {
        match nonce {
            Some(nonce) => self.seen.iter().any(|seen| seen == nonce),
            None => false,
        }
    }

    /// Record a nonce as accepted. A `None` nonce records nothing. Recording the
    /// same nonce twice is idempotent (it is not duplicated in the ring and does
    /// not evict a distinct entry).
    pub(super) fn record(&mut self, nonce: Option<&str>) {
        let Some(nonce) = nonce else {
            return;
        };
        if self.seen.iter().any(|seen| seen == nonce) {
            return;
        }
        if self.seen.len() >= MAX_TRACKED_NONCES {
            self.seen.pop_front();
        }
        self.seen.push_back(nonce.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_nonce_is_never_a_duplicate() {
        let mut tracker = MessageNonceTracker::new();
        assert!(!tracker.is_duplicate(None));
        // Recording a None is a no-op and does not make a later None a dup.
        tracker.record(None);
        assert!(!tracker.is_duplicate(None));
    }

    #[test]
    fn records_then_detects_duplicate() {
        let mut tracker = MessageNonceTracker::new();
        assert!(!tracker.is_duplicate(Some("abc")));
        tracker.record(Some("abc"));
        assert!(tracker.is_duplicate(Some("abc")));
        // A different nonce is still fresh.
        assert!(!tracker.is_duplicate(Some("xyz")));
    }

    #[test]
    fn recording_same_nonce_twice_keeps_single_entry() {
        let mut tracker = MessageNonceTracker::new();
        tracker.record(Some("dup"));
        tracker.record(Some("dup"));
        assert_eq!(tracker.seen.len(), 1);
        assert!(tracker.is_duplicate(Some("dup")));
    }

    #[test]
    fn evicts_oldest_beyond_capacity() {
        let mut tracker = MessageNonceTracker::new();
        for i in 0..(MAX_TRACKED_NONCES + 10) {
            tracker.record(Some(&format!("n{i}")));
        }
        assert_eq!(tracker.seen.len(), MAX_TRACKED_NONCES);
        // The very first nonces were evicted and are treated as fresh again.
        assert!(!tracker.is_duplicate(Some("n0")));
        assert!(!tracker.is_duplicate(Some("n9")));
        // The most recent nonce is still tracked.
        let last = format!("n{}", MAX_TRACKED_NONCES + 9);
        assert!(tracker.is_duplicate(Some(&last)));
    }
}
