//! Optional integration with `cswap` (claude-swap), the operator's external
//! multi-account manager for Claude credentials.
//!
//! cswap owns the shared `~/.claude/.credentials.json` file and decides which
//! Anthropic account is "active" for Claude Code. jcode keeps its own account
//! list in `~/.jcode/auth.json` and switches accounts purely through the
//! in-process active-account override (see [`crate::auth::claude`]); it never
//! rewrites cswap's credential file. This module lets jcode *read* cswap's view
//! so a freshly started session can align its active account to whatever cswap
//! currently has selected, instead of defaulting to a stale cached account and
//! immediately hitting a rate limit.
//!
//! The integration is best-effort and entirely optional: when the `cswap`
//! binary is not on `PATH`, or its output cannot be parsed, every function here
//! returns `None`/empty and callers fall back to jcode's existing behavior.
//! cswap is therefore never a hard dependency of jcode.

use std::process::Command;
use std::time::Duration;

/// A single cswap-managed account, distilled from `cswap ... --json`.
///
/// Only the fields jcode needs to align its own accounts are kept. Accounts are
/// joined to jcode's `anthropic_accounts` by [`email`](Self::email), the only
/// identity stable across both tools (cswap's positional slot numbers and
/// jcode's `claude-N` labels can disagree and are not interchangeable).
#[derive(Debug, Clone)]
pub struct CswapAccount {
    /// Account email, e.g. `dev1@hyfin.app`. The stable join key.
    pub email: String,
    /// Whether cswap currently has this account active.
    pub active: bool,
    /// cswap's usage status string, e.g. `ok`, when present.
    pub usage_status: Option<String>,
}

/// The currently active cswap account, or `None` when cswap is unavailable.
///
/// Reads `cswap status --json` and returns the active account's email. A
/// missing binary, non-zero exit, timeout, or unparseable output all yield
/// `None`, so the caller silently keeps jcode's own selection.
pub fn active_account_email() -> Option<String> {
    let value = run_cswap_json(&["status", "--json"])?;
    // `cswap status --json` => { "active": { "email": "...", ... }, ... }
    value
        .get("active")?
        .get("email")?
        .as_str()
        .map(str::to_string)
}

/// All cswap-managed accounts, or an empty vec when cswap is unavailable.
///
/// Reads `cswap list --json`. Accounts without an email are skipped (they
/// cannot be joined to a jcode account).
pub fn list_accounts() -> Vec<CswapAccount> {
    let Some(value) = run_cswap_json(&["list", "--json"]) else {
        return Vec::new();
    };
    let Some(accounts) = value.get("accounts").and_then(|a| a.as_array()) else {
        return Vec::new();
    };
    accounts
        .iter()
        .filter_map(|account| {
            let email = account.get("email")?.as_str()?.to_string();
            let active = account
                .get("active")
                .and_then(|a| a.as_bool())
                .unwrap_or(false);
            let usage_status = account
                .get("usageStatus")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            Some(CswapAccount {
                email,
                active,
                usage_status,
            })
        })
        .collect()
}

/// Whether the `cswap` binary is available on this host.
pub fn is_available() -> bool {
    run_cswap_json(&["status", "--json"]).is_some()
}

/// Run `cswap <args>` and parse stdout as JSON, or `None` on any failure.
///
/// Bounded by a short timeout guard: cswap is a fast local CLI, so a slow or
/// hung invocation must never block jcode startup. `Command` has no built-in
/// timeout, so the call is spawned on a helper thread and abandoned if it does
/// not finish promptly.
fn run_cswap_json(args: &[&str]) -> Option<serde_json::Value> {
    let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let output = Command::new("cswap").args(&owned).output();
        // Receiver may be gone if we already timed out; ignore the send error.
        let _ = tx.send(output);
    });

    let output = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(output)) => output,
        // Command failed to spawn (cswap not installed) or timed out.
        Ok(Err(_)) | Err(_) => return None,
    };

    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_active_email_from_status_json() {
        let json = serde_json::json!({
            "schemaVersion": 1,
            "active": { "number": 2, "email": "dev1@hyfin.app", "usage": {} }
        });
        assert_eq!(
            json.get("active")
                .and_then(|a| a.get("email"))
                .and_then(|e| e.as_str()),
            Some("dev1@hyfin.app")
        );
    }

    #[test]
    fn parses_accounts_from_list_json() {
        let value = serde_json::json!({
            "schemaVersion": 1,
            "activeAccountNumber": 2,
            "accounts": [
                { "number": 1, "email": "cyuan@hyfin.app", "active": false, "usageStatus": "ok" },
                { "number": 2, "email": "dev1@hyfin.app", "active": true, "usageStatus": "ok" }
            ]
        });
        let accounts: Vec<CswapAccount> = value
            .get("accounts")
            .and_then(|a| a.as_array())
            .unwrap()
            .iter()
            .filter_map(|account| {
                let email = account.get("email")?.as_str()?.to_string();
                let active = account
                    .get("active")
                    .and_then(|a| a.as_bool())
                    .unwrap_or(false);
                let usage_status = account
                    .get("usageStatus")
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
                Some(CswapAccount {
                    email,
                    active,
                    usage_status,
                })
            })
            .collect();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].email, "cyuan@hyfin.app");
        assert!(!accounts[0].active);
        assert_eq!(accounts[1].email, "dev1@hyfin.app");
        assert!(accounts[1].active);
        assert_eq!(accounts[1].usage_status.as_deref(), Some("ok"));
    }

    #[test]
    fn missing_active_email_yields_none() {
        let json = serde_json::json!({ "schemaVersion": 1 });
        assert!(json.get("active").and_then(|a| a.get("email")).is_none());
    }
}
