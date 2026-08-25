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

use crate::session;

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
    let model_note = outcome
        .model
        .as_deref()
        .map(|m| format!(" model={m}"))
        .unwrap_or_default();
    let error = outcome
        .error
        .as_deref()
        .map(|e| format!(" - {e}"))
        .unwrap_or_default();
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
}
