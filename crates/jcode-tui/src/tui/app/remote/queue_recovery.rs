use super::*;

impl App {
    pub(super) fn track_pending_soft_interrupt(&mut self, request_id: u64, content: String) {
        let content_bytes = content.len();
        let content_chars = content.chars().count();
        self.pending_soft_interrupt_requests
            .push((request_id, content.clone()));
        self.pending_soft_interrupts.push(content);
        crate::logging::info(&format!(
            "REMOTE_SOFT_INTERRUPT_TRACK_PENDING id={} content_bytes={} content_chars={} pending_requests={} pending_messages={}",
            request_id,
            content_bytes,
            content_chars,
            self.pending_soft_interrupt_requests.len(),
            self.pending_soft_interrupts.len()
        ));
    }

    pub(super) fn acknowledge_pending_soft_interrupt(&mut self, request_id: u64) -> bool {
        if let Some(index) = self
            .pending_soft_interrupt_requests
            .iter()
            .position(|(id, _)| *id == request_id)
        {
            self.pending_soft_interrupt_requests.remove(index);
            crate::logging::info(&format!(
                "REMOTE_SOFT_INTERRUPT_ACK_MATCHED id={} pending_requests={} pending_messages={}",
                request_id,
                self.pending_soft_interrupt_requests.len(),
                self.pending_soft_interrupts.len()
            ));
            true
        } else {
            if !self.pending_soft_interrupt_requests.is_empty() {
                crate::logging::info(&format!(
                    "REMOTE_SOFT_INTERRUPT_ACK_UNMATCHED id={} pending_requests={} pending_messages={}",
                    request_id,
                    self.pending_soft_interrupt_requests.len(),
                    self.pending_soft_interrupts.len()
                ));
            }
            false
        }
    }

    pub(super) fn clear_pending_soft_interrupt_tracking(&mut self) {
        crate::logging::info(&format!(
            "REMOTE_SOFT_INTERRUPT_TRACKING_CLEAR pending_requests={} pending_messages={}",
            self.pending_soft_interrupt_requests.len(),
            self.pending_soft_interrupts.len()
        ));
        self.pending_soft_interrupts.clear();
        self.pending_soft_interrupt_requests.clear();
    }

    pub(super) fn mark_soft_interrupt_injected(&mut self, content: &str) {
        crate::logging::info(&format!(
            "REMOTE_SOFT_INTERRUPT_MARK_INJECTED content_bytes={} content_chars={} pending_requests={} pending_messages={}",
            content.len(),
            content.chars().count(),
            self.pending_soft_interrupt_requests.len(),
            self.pending_soft_interrupts.len()
        ));
        if self.mark_combined_soft_interrupt_injected(content) {
            return;
        }

        if let Some(index) = self
            .pending_soft_interrupts
            .iter()
            .position(|pending| pending == content)
        {
            self.pending_soft_interrupts.remove(index);
        }

        if let Some(index) = self
            .pending_soft_interrupt_requests
            .iter()
            .position(|(_, pending)| pending == content)
        {
            self.pending_soft_interrupt_requests.remove(index);
        }
    }

    fn mark_combined_soft_interrupt_injected(&mut self, content: &str) -> bool {
        let mut combined = String::new();
        for (index, pending) in self.pending_soft_interrupts.iter().enumerate() {
            if index > 0 {
                combined.push_str("\n\n");
            }
            combined.push_str(pending);

            if combined == content {
                let count = index + 1;
                let removed: Vec<String> = self.pending_soft_interrupts.drain(..count).collect();
                for removed_content in removed {
                    if let Some(request_index) = self
                        .pending_soft_interrupt_requests
                        .iter()
                        .position(|(_, pending)| pending == &removed_content)
                    {
                        self.pending_soft_interrupt_requests.remove(request_index);
                    }
                }
                return true;
            }

            if !content.starts_with(&combined) {
                break;
            }
        }

        false
    }
}

/// Recover an in-flight queued continuation back into the queue.
///
/// A queued follow-up that was already taken from `queued_messages` and handed
/// to `begin_remote_send` lives only in `rate_limit_pending_message` while it
/// is in flight. That pending shape (`is_system` with `auto_retry == false`)
/// has no retry path: the tick resend requires a rate-limit reset timestamp
/// and the disconnect resend requires `auto_retry`. If the connection dies
/// before the turn completes (typically a server reload handoff racing the
/// dispatch), clearing the pending message silently drops the user's queued
/// message (issue #391). Instead, put it back at the front of the queue so it
/// is re-sent once the turn is proven idle after reconnect, which is the
/// queue's contract.
pub(super) fn recover_undelivered_queued_continuation(app: &mut App, reason: &str) -> bool {
    let is_recoverable = app
        .rate_limit_pending_message
        .as_ref()
        .is_some_and(|pending| {
            pending.is_system
                && !pending.auto_retry
                && (!pending.content.trim().is_empty() || pending.system_reminder.is_some())
        });
    if !is_recoverable {
        return false;
    }
    let Some(pending) = app.rate_limit_pending_message.take() else {
        return false;
    };
    app.rate_limit_reset = None;
    // Preserve this submission's idempotency nonce across the re-queue hop. The
    // pending message is dropped here and only its content survives in the
    // queue, so stash (content, nonce) for the immediate re-send to reuse. This
    // is what lets the server deduplicate the re-appended user turn instead of
    // duplicating it (the busy-recovery amplifier, issue #391).
    if let Some(nonce) = pending.submission_nonce.clone()
        && !pending.content.trim().is_empty()
    {
        app.busy_recovered_submission = Some((pending.content.clone(), nonce));
    }
    crate::logging::info(&format!(
        "Recovering in-flight queued continuation into queued follow-ups after {} (content_chars={}, has_reminder={})",
        reason,
        pending.content.chars().count(),
        pending.system_reminder.is_some()
    ));
    if let Some(reminder) = pending.system_reminder {
        app.hidden_queued_system_messages.insert(0, reminder);
    }
    if !pending.content.trim().is_empty() {
        app.queued_messages.insert(0, pending.content);
    }
    true
}

/// Outcome of a bounded busy-rejection recovery attempt.
pub(super) enum BusyRecoveryOutcome {
    /// The queued continuation was recovered back onto the queue; the caller
    /// should re-adopt the running-turn state so the queue re-dispatches once
    /// the turn is proven idle.
    Recovered,
    /// The pending message was not a recoverable queued continuation; the
    /// caller should fall through to its other error handling.
    NotRecoverable,
    /// The per-submission busy-recovery budget is exhausted. The pending
    /// message was dropped and its text restored to the input box (never
    /// silently lost, per issue #391). The caller must NOT re-adopt the turn:
    /// re-adopting would let the next tick re-dispatch and continue the storm.
    BudgetExhausted,
}

/// Bounded busy-rejection recovery of an in-flight queued continuation.
///
/// The server rejects a queued follow-up with "Already processing a message"
/// while a prior turn is still running. The normal recovery re-queues the
/// message and re-adopts the running turn so it re-dispatches once the turn is
/// idle. A reconnect/reload race that leaves the turn permanently "running"
/// from the client's view turns that into an unbounded resend storm (one
/// queued message re-sent 174 times, one send per dispatch tick).
///
/// This variant caps recoveries of the SAME submission (keyed on its
/// idempotency nonce) at `App::BUSY_RECOVERY_MAX_ATTEMPTS`. Below the cap it
/// behaves exactly like `recover_undelivered_queued_continuation`. At the cap
/// it stops re-queuing, restores the user's text to the input box, and reports
/// `BudgetExhausted` so the loop terminates instead of resending forever.
pub(super) fn recover_undelivered_queued_continuation_bounded(
    app: &mut App,
    reason: &str,
) -> BusyRecoveryOutcome {
    let recovery_key = app
        .rate_limit_pending_message
        .as_ref()
        .filter(|pending| {
            pending.is_system
                && !pending.auto_retry
                && (!pending.content.trim().is_empty() || pending.system_reminder.is_some())
        })
        .map(|pending| {
            // Key on the stable submission nonce so the counter follows one
            // logical submission across re-queue hops. Fall back to the content
            // when a nonce is absent (older path / bare reminder) so we still
            // bound it.
            pending
                .submission_nonce
                .clone()
                .unwrap_or_else(|| pending.content.clone())
        });
    let Some(recovery_key) = recovery_key else {
        return BusyRecoveryOutcome::NotRecoverable;
    };

    let attempts = match app.busy_recovery_attempts.as_mut() {
        Some((key, count)) if *key == recovery_key => {
            *count = count.saturating_add(1);
            *count
        }
        _ => {
            // New submission (or the previous one cleared): start its counter.
            app.busy_recovery_attempts = Some((recovery_key.clone(), 1));
            1
        }
    };

    if attempts > App::BUSY_RECOVERY_MAX_ATTEMPTS {
        // Budget exhausted: stop the storm. Drop the pending message, restore
        // the user's text to the input box so nothing is lost, and let the
        // caller fall through to a terminal, idle state.
        let dropped = app.rate_limit_pending_message.take();
        app.rate_limit_reset = None;
        app.busy_recovered_submission = None;
        app.busy_recovery_attempts = None;
        if let Some(pending) = dropped {
            if !pending.content.trim().is_empty() {
                app.last_submitted_input
                    .get_or_insert_with(|| pending.content.clone());
            }
            if let Some(reminder) = pending.system_reminder {
                // A hidden reminder has no input-box home; keep it queued so the
                // continuation is not lost, but the visible content is what the
                // storm was resending, and it is now restored to the box.
                app.hidden_queued_system_messages.insert(0, reminder);
            }
        }
        app.restore_failed_input_to_box();
        crate::logging::warn(&format!(
            "Busy-rejection recovery budget exhausted after {} attempts ({}); stopped re-queuing and restored the message to the input box",
            App::BUSY_RECOVERY_MAX_ATTEMPTS,
            reason
        ));
        app.push_display_message(DisplayMessage::system(format!(
            "🛑 Stopped re-sending your queued message after {} server-busy rejections. It is back in your input box; press Enter to send it again.",
            App::BUSY_RECOVERY_MAX_ATTEMPTS
        )));
        app.set_status_notice("Server stayed busy; message restored to input");
        return BusyRecoveryOutcome::BudgetExhausted;
    }

    if recover_undelivered_queued_continuation(app, reason) {
        BusyRecoveryOutcome::Recovered
    } else {
        // Should not happen (the guard above already proved recoverability), but
        // stay safe and do not claim a recovery that did not occur.
        BusyRecoveryOutcome::NotRecoverable
    }
}

pub(super) fn recover_local_interleave_to_queue(app: &mut App, reason: &str) -> bool {
    let Some(interleave) = app.interleave_message.take() else {
        return false;
    };
    if interleave.trim().is_empty() {
        return false;
    }

    crate::logging::info(&format!(
        "Recovering unsent interleave into queued follow-ups after {}",
        reason
    ));
    app.queued_messages.insert(0, interleave);
    true
}

pub(super) async fn recover_stranded_soft_interrupts(
    app: &mut App,
    remote: &mut RemoteConnection,
) -> bool {
    if app.is_processing || app.pending_soft_interrupts.is_empty() {
        return false;
    }

    let recovered_interrupts = std::mem::take(&mut app.pending_soft_interrupts);
    if recovered_interrupts.is_empty() {
        return false;
    }

    if let Err(err) = remote.cancel_soft_interrupts().await {
        app.pending_soft_interrupts = recovered_interrupts;
        app.push_display_message(DisplayMessage::error(format!(
            "Failed to recover queued interleave message: {}",
            err
        )));
        app.set_status_notice("Queued interleave recovery failed");
        return false;
    }

    crate::logging::info(&format!(
        "Recovering {} stranded soft interrupt(s) into queued follow-ups after turn boundary",
        recovered_interrupts.len()
    ));
    app.pending_soft_interrupt_requests.clear();

    let mut recovered_queue = recovered_interrupts;
    recovered_queue.append(&mut app.queued_messages);
    app.queued_messages = recovered_queue;
    app.set_status_notice("Recovered queued interleave after turn finished");
    true
}
