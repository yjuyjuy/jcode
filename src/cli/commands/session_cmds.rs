//! `jcode session ...` subcommand handlers.
//!
//! Split out of `commands.rs` to keep that file within the code-size ratchet.
//! These are the headless, non-interactive session control-surface verbs
//! (`rename`, `list`, `switch-account`, `set-model`) used by automation and the
//! account-switch orchestrator (ADR 0031).

use anyhow::Result;
use serde::Serialize;

use crate::cli::args::SessionCommand;
use crate::session;

/// Dispatch a parsed `jcode session <subcommand>` to its handler. Kept beside
/// the handlers so `dispatch.rs` stays a thin router (and within its code-size
/// ratchet). `pub(crate)` because `SessionCommand` is a crate-internal arg enum.
pub(crate) async fn run_session_command(subcmd: SessionCommand) -> Result<()> {
    match subcmd {
        SessionCommand::Rename {
            session,
            name,
            clear,
            json,
        } => run_session_rename_command(&session, name.as_deref(), clear, json),
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

#[derive(Serialize)]
struct SessionRenameOutput {
    session_id: String,
    display_name: String,
    title: Option<String>,
    cleared: bool,
}

pub(crate) fn run_session_rename_command(
    session_ref: &str,
    name: Option<&str>,
    clear: bool,
    json: bool,
) -> Result<()> {
    let resolved_id = session::find_session_by_name_or_id(session_ref)?;
    let mut session = session::Session::load(&resolved_id)?;

    if clear {
        session.rename_title(None);
    } else {
        let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
            anyhow::bail!("Provide a session name or use --clear");
        };
        session.rename_title(Some(name.to_string()));
    }

    session.save()?;
    crate::tui::session_picker::invalidate_session_list_cache();

    let output = SessionRenameOutput {
        session_id: session.id.clone(),
        display_name: session.display_name().to_string(),
        title: session.display_title().map(ToOwned::to_owned),
        cleared: clear,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if clear {
        println!(
            "Cleared custom name for session {} ({}).",
            output.display_name, output.session_id
        );
    } else if let Some(title) = output.title.as_deref() {
        println!(
            "Renamed session {} ({}) to \"{}\".",
            output.display_name, output.session_id, title
        );
    }

    Ok(())
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
pub(crate) async fn run_session_list_command(json: bool) -> Result<()> {
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
pub(crate) async fn run_session_switch_account_command(
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
                .unwrap_or_else(String::new);
            let error = outcome
                .error
                .as_deref()
                .map(|e| format!(" - {e}"))
                .unwrap_or_else(String::new);
            println!(
                "{}: {} account={}{}{}",
                outcome.session_id, status, account, model_note, error
            );
        }
    }

    // Exit non-zero when any target failed so scripts can gate on it.
    if results.iter().any(|outcome| !outcome.ok) {
        anyhow::bail!("one or more sessions failed to switch");
    }
    Ok(())
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
#[path = "session_cmds_tests.rs"]
mod tests;
