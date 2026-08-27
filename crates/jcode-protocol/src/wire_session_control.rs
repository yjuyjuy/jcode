//! Session control-surface wire types.
//!
//! Split out of `wire.rs` to keep that file within the code-size budget. These
//! describe the headless account/model switch control surface (ADR 0031): a
//! session's live identity and the per-session outcome of a switch request.

use serde::{Deserialize, Serialize};

/// One live session's control-surface identity: provider, account, and model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionControlInfo {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub friendly_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Account label the session's active provider is pinned to, or `None` when
    /// it follows the process-global active account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort the session's active provider will use for the next
    /// request, or `None` when the provider has no notion of effort (or none is
    /// configured). Additive field: an older client/daemon that predates it
    /// simply omits it, so the wire stays back-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Size of the session's stored record, in bytes: a cheap, monotonic proxy
    /// for how much conversation the session holds (the same measure as
    /// `SessionInfo.transcript_bytes` in the harness API). `None` when the
    /// daemon could not stat the record. Additive and back-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_bytes: Option<u64>,
    /// True when a turn is currently running for this session. A switch is still
    /// accepted; it is adopted on the next turn (drain semantics).
    #[serde(default)]
    pub is_processing: bool,
}

/// Per-session outcome of an account (and optional model) switch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSwitchOutcome {
    pub session_id: String,
    /// True when the switch was applied (or queued to apply on the next turn).
    pub ok: bool,
    /// The account label the session now targets, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The model the session now targets, when a model switch was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// True when the switch was accepted but deferred because a turn was in
    /// flight; it applies on that session's next turn.
    #[serde(default)]
    pub deferred: bool,
    /// Failure reason when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
