//! Per-session account label resolution for account display surfaces.
//!
//! Split out of `tui_state.rs` to keep that file within the code-size budget.
//! Backs the floating account box and the overscroll status strip: each names
//! THIS session's account (ADR 0031).

use super::App;

impl App {
    /// The account label to display for THIS session in account surfaces (the
    /// floating account box and the overscroll status line).
    ///
    /// Returns the session's per-instance account pin when the live provider is
    /// pinned to one (ADR 0031). When the session follows the process-global
    /// active account (no pin), resolves that global active label for the active
    /// provider so the surface shows a concrete name (e.g. `claude-2`) instead of
    /// nothing. Providers without multiple accounts, and remote sessions (whose
    /// account identity is not carried to this client), return `None`; the caller
    /// then renders an explicit non-empty placeholder rather than a blank.
    ///
    /// Pair with [`App::session_account_is_pinned`] to distinguish a pinned label
    /// from a followed-global one in the display.
    pub(crate) fn session_account_label(&self) -> Option<String> {
        // Remote sessions run an inert local provider whose account_label would
        // read this host's auth.json, which is not the remote session's account.
        // Report nothing rather than a misleading local label.
        if self.is_remote {
            return None;
        }
        if let Some(pinned) = self.provider.account_label() {
            return Some(pinned);
        }
        // No pin: fall back to the resolved process-global active account for the
        // active provider so the label is never blank when an account exists.
        // Only Anthropic and OpenAI support multiple named accounts today.
        match self.provider.name() {
            name if name.eq_ignore_ascii_case("Claude") => {
                crate::auth::claude::active_account_label()
            }
            name if name.eq_ignore_ascii_case("OpenAI") => {
                crate::auth::codex::active_account_label()
            }
            _ => None,
        }
    }

    /// Whether [`App::session_account_label`] is a per-session pin (true) rather
    /// than the followed process-global active account (false). Used to mark the
    /// followed-global case explicitly in account surfaces.
    pub(crate) fn session_account_is_pinned(&self) -> bool {
        !self.is_remote && self.provider.account_label().is_some()
    }
}
