//! Session account-switch control surface (ADR 0031, Phase 1).
//!
//! An external orchestrator (quota-axi's fenced `switch` verb) needs to
//! actuate account switches on *live* jcode sessions without terminal
//! injection. jcode's `auth.json` active-account label is process-global and
//! only affects new sessions, so flipping the store cannot switch a session
//! that is already running. This module exposes a subscription-free control
//! surface over the daemon socket that:
//!
//! - lists live sessions with their current provider, account, and model, and
//! - switches a session's account (optionally with an atomic model change for
//!   the provider-crossing case) either per-session or across all sessions.
//!
//! Every switch honors drain semantics: it is applied immediately when the
//! session is idle, or deferred to the session's next turn when a turn is in
//! flight, so a switch never interrupts a running turn.

use crate::agent::Agent;
use crate::protocol::{ServerEvent, SessionControlInfo, SessionSwitchOutcome};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};

use super::SwarmMember;

type SessionAgents = Arc<RwLock<HashMap<String, Arc<Mutex<Agent>>>>>;

/// What a switch request should change on each target session.
enum SwitchKind {
    /// Account only, keeping the current model.
    AccountOnly { account: String },
    /// Account and model together (provider-crossing case). `model` is a model
    /// spec string parsed by the active provider orchestrator.
    AccountAndModel { account: String, model: String },
}

impl SwitchKind {
    fn account(&self) -> &str {
        match self {
            SwitchKind::AccountOnly { account } => account,
            SwitchKind::AccountAndModel { account, .. } => account,
        }
    }

    fn model(&self) -> Option<&str> {
        match self {
            SwitchKind::AccountOnly { .. } => None,
            SwitchKind::AccountAndModel { model, .. } => Some(model),
        }
    }
}

/// Handle `Request::ListSessions`: enumerate live sessions with their provider,
/// account, and model. Every live daemon session is reported, including
/// headless and swarm sessions, since they consume account quota too.
pub(super) async fn handle_list_sessions(
    id: u64,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let (agents, members) = live_session_agents(sessions, swarm_members).await;
    let mut out = Vec::with_capacity(agents.len());
    for (session_id, agent_arc) in &agents {
        let member = members.get(session_id);
        let member_running = member.map(|m| m.status.as_str()) == Some("running");
        // A busy session holds its own lock during a turn. Never block on it
        // here: report what we can and mark it processing so the caller knows
        // the model/account fields may be a beat stale.
        let info = match agent_arc.try_lock() {
            Ok(agent) => SessionControlInfo {
                session_id: session_id.clone(),
                friendly_name: member.and_then(|m| m.friendly_name.clone()),
                provider: Some(agent.provider_name()),
                account: agent.account_label(),
                model: Some(agent.provider_model()),
                effort: agent.reasoning_effort(),
                transcript_bytes: session_transcript_bytes(session_id),
                is_processing: member_running,
            },
            Err(_) => SessionControlInfo {
                session_id: session_id.clone(),
                friendly_name: member.and_then(|m| m.friendly_name.clone()),
                provider: None,
                account: None,
                model: None,
                effort: None,
                transcript_bytes: session_transcript_bytes(session_id),
                is_processing: true,
            },
        };
        out.push(info);
    }
    let _ = client_event_tx.send(ServerEvent::SessionList { id, sessions: out });
}

/// Handle `Request::SwitchSessionAccount`: switch one or all live sessions to a
/// different account for the active provider, no model change.
pub(super) async fn handle_switch_session_account(
    id: u64,
    session_id: Option<String>,
    account: String,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    switch_sessions(
        id,
        session_id,
        SwitchKind::AccountOnly { account },
        sessions,
        swarm_members,
        client_event_tx,
    )
    .await;
}

/// Handle `Request::SwitchSessionAccountModel`: atomically switch account and
/// model together (the provider-crossing case).
pub(super) async fn handle_switch_session_account_model(
    id: u64,
    session_id: Option<String>,
    account: String,
    model: String,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    switch_sessions(
        id,
        session_id,
        SwitchKind::AccountAndModel { account, model },
        sessions,
        swarm_members,
        client_event_tx,
    )
    .await;
}

async fn switch_sessions(
    id: u64,
    target: Option<String>,
    kind: SwitchKind,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) {
    let kind = Arc::new(kind);

    // Resolve the target agents. A single-session request that names an unknown
    // session is reported as one failed outcome so the caller still gets a
    // result row rather than an empty list.
    let targets: Vec<(String, Arc<Mutex<Agent>>)> = match &target {
        Some(session_id) => {
            let agent = { sessions.read().await.get(session_id).cloned() };
            match agent {
                Some(agent) => vec![(session_id.clone(), agent)],
                None => {
                    let outcome = SessionSwitchOutcome {
                        session_id: session_id.clone(),
                        ok: false,
                        account: Some(kind.account().to_string()),
                        model: kind.model().map(str::to_string),
                        deferred: false,
                        error: Some(format!("session not found: {session_id}")),
                    };
                    let _ = client_event_tx.send(ServerEvent::SessionSwitchResult {
                        id,
                        results: vec![outcome],
                    });
                    return;
                }
            }
        }
        None => {
            let (agents, _members) = live_session_agents(sessions, swarm_members).await;
            agents
        }
    };

    let mut results = Vec::with_capacity(targets.len());
    let mut deferred: Vec<(String, Arc<Mutex<Agent>>)> = Vec::new();

    for (session_id, agent_arc) in targets {
        let applied = match agent_arc.try_lock() {
            Ok(mut agent) => {
                results.push(apply_switch(&session_id, &mut agent, &kind, false));
                true
            }
            Err(_) => false,
        };
        if !applied {
            // The session is mid-turn. Defer: the switch is applied when the
            // turn releases the lock, so it takes effect on the next turn
            // without interrupting the one in flight (drain semantics).
            deferred.push((session_id, agent_arc));
        }
    }

    // Spawn one deferred applier per busy session. Report these as accepted +
    // deferred now; the actual apply happens when each turn drains.
    for (session_id, agent_arc) in deferred {
        results.push(SessionSwitchOutcome {
            session_id: session_id.clone(),
            ok: true,
            account: Some(kind.account().to_string()),
            model: kind.model().map(str::to_string),
            deferred: true,
            error: None,
        });
        let kind = Arc::clone(&kind);
        tokio::spawn(async move {
            let mut agent = agent_arc.lock().await;
            let outcome = apply_switch(&session_id, &mut agent, &kind, true);
            if let Some(error) = outcome.error {
                crate::logging::warn(&format!(
                    "Deferred account switch for session {session_id} failed after drain: {error}"
                ));
            } else {
                crate::logging::info(&format!(
                    "Applied deferred account switch for session {session_id} on next turn"
                ));
            }
        });
    }

    let _ = client_event_tx.send(ServerEvent::SessionSwitchResult { id, results });
}

/// Apply the switch to a locked agent and build its outcome row.
fn apply_switch(
    session_id: &str,
    agent: &mut Agent,
    kind: &SwitchKind,
    deferred: bool,
) -> SessionSwitchOutcome {
    let account = kind.account().to_string();
    let result = match kind {
        SwitchKind::AccountOnly { account } => agent.set_account_label(Some(account.clone())),
        SwitchKind::AccountAndModel { account, model } => {
            agent.switch_account_and_model(Some(account.clone()), model)
        }
    };
    match result {
        Ok(()) => {
            crate::logging::event_info(
                "SERVER_SESSION_ACCOUNT_SWITCH",
                vec![
                    ("session_id", session_id.to_string()),
                    ("account", account.clone()),
                    (
                        "model",
                        kind.model().map(str::to_string).unwrap_or_default(),
                    ),
                    ("deferred", deferred.to_string()),
                ],
            );
            SessionSwitchOutcome {
                session_id: session_id.to_string(),
                ok: true,
                account: Some(account),
                model: Some(agent.provider_model()),
                deferred,
                error: None,
            }
        }
        Err(error) => SessionSwitchOutcome {
            session_id: session_id.to_string(),
            ok: false,
            account: Some(account),
            model: kind.model().map(str::to_string),
            deferred,
            error: Some(format!("{error:#}")),
        },
    }
}

/// Size of a session's persisted record in bytes, a cheap monotonic proxy for
/// "how much conversation is in here" that the `session list` health view
/// surfaces. Statting the file avoids loading or locking the live agent, so a
/// busy (mid-turn) session still reports a context size. Best-effort: an
/// unreadable, missing, or non-id-shaped session simply reports `None` rather
/// than failing the list. Mirrors the harness API's `transcript_bytes` (see
/// `crates/jcode-harness-api-server/src/translate.rs`).
fn session_transcript_bytes(session_id: &str) -> Option<u64> {
    // Session ids are `session_<name>_<millis>_<hex>`; reject anything with a
    // path separator or parent reference so this can never stat outside the
    // sessions directory.
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    let path = crate::storage::jcode_dir()
        .ok()?
        .join("sessions")
        .join(format!("{session_id}.json"));
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

/// Collect every live session agent in the daemon, plus a snapshot of
/// swarm-member metadata (friendly names / status). Unlike the debug picker's
/// connected-only view, the account-switch control surface targets *every* live
/// session because headless and swarm sessions consume account quota too, so
/// they are legitimate switch targets. Never holds more than one shared-state
/// lock at a time, matching the debug snapshot's deadlock-avoidance discipline.
async fn live_session_agents(
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> (
    Vec<(String, Arc<Mutex<Agent>>)>,
    HashMap<String, SwarmMember>,
) {
    let agents = {
        let sessions_guard = sessions.read().await;
        sessions_guard
            .iter()
            .map(|(session_id, agent)| (session_id.clone(), Arc::clone(agent)))
            .collect()
    };
    let members = swarm_members.read().await.clone();
    (agents, members)
}

#[cfg(test)]
#[path = "session_control_tests.rs"]
mod session_control_tests;
