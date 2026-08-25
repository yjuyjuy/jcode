//! CLI entry points for the live-session account-switch control surface
//! (ADR 0031, Phase 1).
//!
//! These are the headless commands an external orchestrator (quota-axi's fenced
//! `switch` verb, or firstmate's rotate path) drives to actuate an account
//! rotation onto running jcode sessions without terminal injection:
//! `jcode session list` enumerates live sessions, and
//! `jcode session switch-account` moves one or all of them to a new account
//! (optionally with an atomic model change). The switch mechanics live in the
//! daemon (`crates/jcode-app-core/src/server/session_control.rs`); this module
//! is the thin CLI presentation and exit-status layer over the socket client.

use anyhow::Result;
use serde::Serialize;

use crate::cli::args::SessionCommand;
use crate::session;

/// Dispatch a parsed `jcode session <subcommand>` to its handler. Kept beside
/// the session control-surface handlers so `dispatch.rs` stays a thin router
/// (and within its code-size ratchet).
pub(crate) async fn run_session_command(subcmd: SessionCommand) -> Result<()> {
    match subcmd {
        SessionCommand::Rename {
            session,
            name,
            clear,
            json,
        } => super::run_session_rename_command(&session, name.as_deref(), clear, json),
        SessionCommand::List { json } => run_session_list_command(json).await,
        SessionCommand::SwitchAccount {
            session,
            all,
            account,
            model,
            json,
        } => {
            run_session_switch_account_command(session, all, &account, model.as_deref(), json).await
        }
        SessionCommand::SetModel {
            model,
            effort,
            session,
            socket,
            json,
        } => run_session_set_model_command(&model, effort.as_deref(), session, socket, json).await,
    }
}

/// One live session row for `jcode session list`.
#[derive(Debug, Serialize)]
struct SessionListRow {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    is_processing: bool,
}

/// `jcode session list`: enumerate live sessions with provider/account/model.
/// Headless control surface for the account-switch orchestrator (ADR 0031).
pub async fn run_session_list_command(json: bool) -> Result<()> {
    let mut client = crate::server::Client::connect().await?;
    let sessions = client.list_sessions().await?;

    let rows: Vec<SessionListRow> = sessions
        .into_iter()
        .map(|info| SessionListRow {
            session_id: info.session_id,
            name: info.friendly_name,
            provider: info.provider,
            account: info.account,
            model: info.model,
            is_processing: info.is_processing,
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No live sessions.");
        return Ok(());
    }

    for row in &rows {
        let name = row.name.as_deref().unwrap_or("-");
        let provider = row.provider.as_deref().unwrap_or("?");
        let account = row.account.as_deref().unwrap_or("(default)");
        let model = row.model.as_deref().unwrap_or("?");
        let busy = if row.is_processing { " [busy]" } else { "" };
        println!(
            "{} ({})  provider={} account={} model={}{}",
            row.session_id, name, provider, account, model, busy
        );
    }
    Ok(())
}

/// `jcode session switch-account`: switch a live session's account, optionally
/// with an atomic model change, per-session or all-sessions. Reports per-session
/// success/failure. Part of the account-switch control surface (ADR 0031).
pub async fn run_session_switch_account_command(
    session: Option<String>,
    all: bool,
    account: &str,
    model: Option<&str>,
    json: bool,
) -> Result<()> {
    // Resolve a short name / partial id to a full session id client-side so the
    // daemon (which keys sessions by full id) receives an exact target. `--all`
    // skips resolution and switches every live session.
    let target: Option<String> = if all {
        None
    } else {
        let session_ref = session
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Provide a session or use --all"))?;
        Some(
            session::find_session_by_name_or_id(session_ref)
                .unwrap_or_else(|_| session_ref.to_string()),
        )
    };

    let mut client = crate::server::Client::connect().await?;
    let results = match model {
        Some(model) => {
            client
                .switch_session_account_model(target.as_deref(), account, model)
                .await?
        }
        None => {
            client
                .switch_session_account(target.as_deref(), account)
                .await?
        }
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        if results.is_empty() {
            println!("No live sessions matched.");
        }
        for outcome in &results {
            println!("{}", render_switch_outcome_line(outcome));
        }
    }

    // Exit non-zero when any target failed so scripts can gate on it.
    if switch_results_any_failed(&results) {
        anyhow::bail!("one or more sessions failed to switch");
    }
    Ok(())
}

/// Render one per-session switch outcome as a human-readable status line.
///
/// Pure so the orchestrator-facing wording (which the captain report is built
/// from) can be asserted without a live daemon: a deferred switch is still a
/// success, a failure carries its reason, and an atomic model switch names the
/// model it moved to.
fn render_switch_outcome_line(outcome: &crate::protocol::SessionSwitchOutcome) -> String {
    let status = if outcome.ok {
        if outcome.deferred {
            "ok (deferred to next turn)"
        } else {
            "ok"
        }
    } else {
        "failed"
    };
    let account = outcome.account.as_deref().unwrap_or("?");
    let model_note = match outcome.model.as_deref() {
        Some(model) => format!(" model={model}"),
        None => String::new(),
    };
    let error = match outcome.error.as_deref() {
        Some(error) => format!(" - {error}"),
        None => String::new(),
    };
    format!(
        "{}: {} account={}{}{}",
        outcome.session_id, status, account, model_note, error
    )
}

/// Whether any per-session switch outcome failed. The CLI exits non-zero when
/// this is true so an orchestrator (quota-axi's fenced `switch` verb, or
/// firstmate's rotate path) can gate on the process exit status: a single
/// unknown-label refusal must fail the whole invocation, never pass silently.
fn switch_results_any_failed(results: &[crate::protocol::SessionSwitchOutcome]) -> bool {
    results.iter().any(|outcome| !outcome.ok)
}

/// Does the applied (resolved) model reflect the requested model spec?
///
/// The request may carry a provider *route* prefix (e.g. `claude-api:`), which
/// selects the runtime and is consumed - the applied model is then the bare id.
/// Either side may also carry an explicit `@pin` provider suffix. We treat the
/// request as satisfied when the bare model ids match after removing an optional
/// leading route prefix and an optional trailing `@pin`. This lets a plain
/// `deepseek-v4-flash` request verify against a plain applied id (the fleet's
/// common case) while still tolerating route-prefixed and pinned specs, yet it
/// still catches a genuinely different model being applied (silent aliasing).
fn request_model_matches(requested: &str, applied: &str) -> bool {
    fn bare(spec: &str) -> &str {
        // Drop a leading "<route>:" prefix. Route ids (e.g. `claude-api`,
        // `openai-oauth`) never contain '/', which distinguishes them from
        // OpenRouter-style `vendor/model` ids that legitimately contain '/'.
        let after_route = match spec.split_once(':') {
            Some((prefix, rest)) if !prefix.is_empty() && !prefix.contains('/') => rest,
            _ => spec,
        };
        // Drop a trailing "@pin" provider suffix.
        after_route.split('@').next().unwrap_or(after_route)
    }
    requested.eq_ignore_ascii_case(applied) || bare(requested).eq_ignore_ascii_case(bare(applied))
}

/// Machine-readable result of `jcode session set-model`.
#[derive(Debug, Serialize)]
struct SessionSetModelOutput {
    session_id: Option<String>,
    requested_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_effort: Option<String>,
    applied_model: String,
    applied_provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    applied_effort: Option<String>,
    verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `jcode session set-model`: set a live session's model and (optionally)
/// reasoning effort non-interactively against the running server, then verify
/// the change by reading the session state back over the debug socket.
///
/// This is the reliable, automation-safe replacement for typing `/model <X>`
/// into the TUI composer, which races the slash-command autocomplete popup and
/// can silently no-op. The flow is: (1) send the effort-aware `set_model:` debug
/// verb, which applies model+effort atomically and returns the resulting applied
/// `{model, provider, effort}`; (2) independently re-read the session `state`;
/// (3) confirm the durable state equals the applied result and reflects the
/// requested model. Any mismatch, a rejected model/effort, or a missing debug
/// socket exits non-zero with a clear, machine-readable message - never a silent
/// pending no-op. Idempotent: the same model+effort applied twice yields the
/// same verified end state.
pub(crate) async fn run_session_set_model_command(
    model: &str,
    effort: Option<&str>,
    session: Option<String>,
    socket: Option<String>,
    json: bool,
) -> Result<()> {
    let model = model.trim();
    if model.is_empty() {
        anyhow::bail!("set-model requires a non-empty model");
    }
    let effort = effort.map(str::trim).filter(|e| !e.is_empty());

    // Resolve a short name / partial id to a full session id client-side so the
    // daemon (which keys live sessions by full id) receives an exact target.
    // Fall back to the raw string (it may already be a full/live id the local
    // session store has not persisted). Omitting the session lets the server
    // target its single active session and error if more than one is live.
    let target: Option<String> = session.as_deref().map(|session_ref| {
        session::find_session_by_name_or_id(session_ref).unwrap_or_else(|_| session_ref.to_string())
    });

    // Build the effort-aware `set_model:` JSON payload. A JSON payload is used
    // (rather than a delimited string) because model specs legitimately contain
    // both ':' and '@', so no delimiter char can separate model from effort.
    let mut payload = serde_json::json!({ "model": model });
    if let Some(effort) = effort {
        payload["effort"] = serde_json::json!(effort);
    }
    let set_cmd = format!("set_model:{}", serde_json::to_string(&payload)?);

    // Step 1: apply. A rejected model/effort returns ok=false with the provider
    // error; surface it and exit non-zero (loud, never a silent no-op).
    let set_reply =
        crate::cli::debug::send_debug_command(&set_cmd, target.as_deref(), socket.as_deref())
            .await?;
    if !set_reply.ok {
        report_set_model_failure(
            json,
            target.as_deref(),
            model,
            effort,
            None,
            &set_reply.output,
        )?;
        anyhow::bail!("set-model failed: {}", set_reply.output);
    }
    let applied: serde_json::Value = serde_json::from_str(&set_reply.output).map_err(|e| {
        anyhow::anyhow!(
            "set-model: could not parse server apply response as JSON: {e}: {}",
            set_reply.output
        )
    })?;
    let applied_model = applied
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let applied_provider = applied
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let applied_effort = applied
        .get("effort")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Step 2: independent readback of the persisted session state. This is the
    // gate that would have caught the fleet incident: a set that no-ops leaves
    // the old model/effort visible here.
    let state_reply =
        crate::cli::debug::send_debug_command("state", target.as_deref(), socket.as_deref())
            .await?;
    if !state_reply.ok {
        report_set_model_failure(
            json,
            target.as_deref(),
            model,
            effort,
            Some((&applied_model, &applied_provider, applied_effort.as_deref())),
            &format!("state readback failed: {}", state_reply.output),
        )?;
        anyhow::bail!("set-model verification failed: {}", state_reply.output);
    }
    let state: serde_json::Value = serde_json::from_str(&state_reply.output).map_err(|e| {
        anyhow::anyhow!(
            "set-model: could not parse server state response as JSON: {e}: {}",
            state_reply.output
        )
    })?;
    let state_session_id = state
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let state_model = state
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let state_effort = state
        .get("effort")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Step 3: verify. Collect every discrepancy so the message names all of them.
    let mut problems: Vec<String> = Vec::new();
    if state_model != applied_model {
        problems.push(format!(
            "persisted model '{}' does not match applied model '{}'",
            state_model, applied_model
        ));
    }
    if state_effort != applied_effort {
        problems.push(format!(
            "persisted effort {:?} does not match applied effort {:?}",
            state_effort, applied_effort
        ));
    }
    if !request_model_matches(model, &state_model) {
        problems.push(format!(
            "requested model '{}' is not reflected by session model '{}'",
            model, state_model
        ));
    }
    if effort.is_some() && state_effort.is_none() {
        problems.push(format!(
            "requested effort '{}' but session reports no effort set",
            effort.unwrap_or("")
        ));
    }

    if !problems.is_empty() {
        let joined = problems.join("; ");
        report_set_model_failure(
            json,
            state_session_id.as_deref().or(target.as_deref()),
            model,
            effort,
            Some((&state_model, &applied_provider, state_effort.as_deref())),
            &joined,
        )?;
        anyhow::bail!("set-model verification failed: {joined}");
    }

    let output = SessionSetModelOutput {
        session_id: state_session_id.or(target),
        requested_model: model.to_string(),
        requested_effort: effort.map(str::to_string),
        applied_model: state_model,
        applied_provider,
        applied_effort: state_effort,
        verified: true,
        error: None,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        let session_label = output.session_id.as_deref().unwrap_or("(active)");
        let effort_label = output.applied_effort.as_deref().unwrap_or("(default)");
        println!(
            "session {}: verified model={} provider={} effort={}",
            session_label, output.applied_model, output.applied_provider, effort_label
        );
    }
    Ok(())
}

/// Emit a machine-readable failure record for `set-model` before the command
/// exits non-zero, so automation can parse the outcome on stderr.
fn report_set_model_failure(
    json: bool,
    session_id: Option<&str>,
    requested_model: &str,
    requested_effort: Option<&str>,
    applied: Option<(&str, &str, Option<&str>)>,
    error: &str,
) -> Result<()> {
    if json {
        let (applied_model, applied_provider, applied_effort) = match applied {
            Some((m, p, e)) => (m.to_string(), p.to_string(), e.map(str::to_string)),
            None => (String::new(), String::new(), None),
        };
        let output = SessionSetModelOutput {
            session_id: session_id.map(str::to_string),
            requested_model: requested_model.to_string(),
            requested_effort: requested_effort.map(str::to_string),
            applied_model,
            applied_provider,
            applied_effort,
            verified: false,
            error: Some(error.to_string()),
        };
        eprintln!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        eprintln!("set-model failed: {error}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Account-switch actuation CLI plumbing (ADR 0031). These assert the
    // orchestrator-facing render/exit contract without a live daemon: the switch
    // itself is exercised end-to-end by the server unit tests
    // (crates/jcode-app-core/src/server/session_control_tests.rs) and
    // scripts/asw_session_control_e2e.sh. The captain report line
    // ("account exhausted ..., rotated to claude-2, ...") is built from these
    // rows, so the wording here is load-bearing.

    fn switch_outcome(
        session_id: &str,
        ok: bool,
        account: Option<&str>,
        model: Option<&str>,
        deferred: bool,
        error: Option<&str>,
    ) -> crate::protocol::SessionSwitchOutcome {
        crate::protocol::SessionSwitchOutcome {
            session_id: session_id.to_string(),
            ok,
            account: account.map(str::to_string),
            model: model.map(str::to_string),
            deferred,
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn render_switch_outcome_line_reports_applied_account() {
        let line = render_switch_outcome_line(&switch_outcome(
            "sess-a",
            true,
            Some("claude-2"),
            None,
            false,
            None,
        ));
        assert_eq!(line, "sess-a: ok account=claude-2");
    }

    #[test]
    fn render_switch_outcome_line_marks_deferred_switch_as_ok() {
        // A busy session defers to its next turn; the orchestrator must still read
        // this as a success, not a failure, so the "ok" prefix is required.
        let line = render_switch_outcome_line(&switch_outcome(
            "sess-b",
            true,
            Some("claude-2"),
            None,
            true,
            None,
        ));
        assert_eq!(line, "sess-b: ok (deferred to next turn) account=claude-2");
    }

    #[test]
    fn render_switch_outcome_line_names_atomic_model_switch() {
        let line = render_switch_outcome_line(&switch_outcome(
            "sess-c",
            true,
            Some("openai-2"),
            Some("openai-oauth:gpt-5"),
            false,
            None,
        ));
        assert_eq!(line, "sess-c: ok account=openai-2 model=openai-oauth:gpt-5");
    }

    #[test]
    fn render_switch_outcome_line_carries_unknown_label_error() {
        // The unknown-label refusal path: the provider runtime rejects a label
        // that is not in auth.json, and its reason must reach the operator
        // verbatim.
        let line = render_switch_outcome_line(&switch_outcome(
            "sess-d",
            false,
            Some("no-such-account"),
            None,
            false,
            Some("Cannot pin Anthropic account 'no-such-account'"),
        ));
        assert_eq!(
            line,
            "sess-d: failed account=no-such-account - Cannot pin Anthropic account 'no-such-account'"
        );
    }

    #[test]
    fn switch_results_any_failed_flags_a_single_failure() {
        // One unknown-label refusal in an --all sweep must fail the whole command
        // so the orchestrator's exit-status gate catches it.
        let results = vec![
            switch_outcome("sess-a", true, Some("claude-2"), None, false, None),
            switch_outcome(
                "sess-b",
                false,
                Some("claude-2"),
                None,
                false,
                Some("no account"),
            ),
        ];
        assert!(switch_results_any_failed(&results));
    }

    #[test]
    fn switch_results_any_failed_passes_when_all_ok() {
        let results = vec![
            switch_outcome("sess-a", true, Some("claude-2"), None, false, None),
            switch_outcome("sess-b", true, Some("claude-2"), None, true, None),
        ];
        assert!(!switch_results_any_failed(&results));
    }

    #[test]
    fn switch_results_any_failed_treats_empty_as_no_failure() {
        // No live session matched: an empty result set is not a failure, so
        // `--all` on an idle fleet exits zero rather than erroring.
        assert!(!switch_results_any_failed(&[]));
    }

    #[test]
    fn request_model_matches_plain_and_prefixed_and_pinned() {
        // Exact match (the common fleet case: a plain id in, a plain id back).
        assert!(request_model_matches(
            "deepseek-v4-flash",
            "deepseek-v4-flash"
        ));
        // A route prefix on the request is consumed; the applied id is bare.
        assert!(request_model_matches(
            "claude-api:claude-fable-5",
            "claude-fable-5"
        ));
        // An explicit @pin on the applied model still matches the bare request.
        assert!(request_model_matches("z-ai/glm-5.2", "z-ai/glm-5.2@Novita"));
        // Case-insensitive.
        assert!(request_model_matches("Claude-Fable-5", "claude-fable-5"));
        // OpenRouter vendor/model ids (which contain '/') are not mistaken for a
        // route prefix.
        assert!(request_model_matches("z-ai/glm-5.2", "z-ai/glm-5.2"));
    }

    #[test]
    fn request_model_matches_rejects_a_different_model() {
        // The whole point: a genuinely different applied model must NOT verify,
        // so a silent alias/no-op is caught loudly.
        assert!(!request_model_matches(
            "deepseek-v4-flash",
            "claude-opus-4-6"
        ));
        assert!(!request_model_matches(
            "claude-api:claude-fable-5",
            "claude-opus-4-6"
        ));
    }
}
