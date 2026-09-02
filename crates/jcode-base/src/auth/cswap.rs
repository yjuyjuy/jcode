//! One-way integration with `cswap` (claude-swap), the external manager that
//! owns `~/.claude/.credentials.json` and decides which Anthropic account is
//! active on this host.
//!
//! jcode never picks an account for that file. When a request served from the
//! file hits a usage-limit 429, the provider runtime asks cswap for a single
//! decision tick and then re-reads whatever cswap left in the file. The
//! integration is best-effort: when `cswap` is not on `PATH`, times out, or
//! errors, [`auto_switch_once`] returns `None` and the caller carries on.

use std::time::Duration;
/// Outcome of one `cswap auto --once` decision tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoOnceOutcome {
    /// cswap switched the credential file to another account (exit 0).
    Switched,
    /// cswap saw no reason to switch (exit 2).
    NoAction,
    /// cswap wanted to switch but had no viable target (exit 3).
    Blocked,
    /// cswap reported an error (exit 1 or any other status).
    Error,
}

/// Command used for the nudge. Tests point `JCODE_CSWAP_COMMAND` at a stub;
/// production always runs `cswap` from `PATH`.
fn auto_once_program() -> String {
    if cfg!(any(test, feature = "test-support"))
        && let Ok(program) = std::env::var("JCODE_CSWAP_COMMAND")
    {
        return program;
    }
    "cswap".to_string()
}

/// Upper bound on one nudge: cswap polls usage over the network before it
/// decides, so allow more than the status probe but never stall a retry loop.
const AUTO_ONCE_TIMEOUT: Duration = Duration::from_secs(20);

/// Ask cswap for one decision tick (`cswap auto --once --json`) after a
/// usage-limit error. jcode makes no selection of its own: cswap owns the
/// credential file and the switching policy, so this is a nudge, not a
/// decision. `None` when cswap is not installed or the tick times out; the
/// caller carries on with its ordinary retry in either case.
pub async fn auto_switch_once() -> Option<AutoOnceOutcome> {
    let mut command = tokio::process::Command::new(auto_once_program());
    command
        .args(["auto", "--once", "--json"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(AUTO_ONCE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            crate::logging::info(&format!("cswap nudge skipped: cannot run cswap ({err})"));
            return None;
        }
        Err(_) => {
            crate::logging::warn("cswap nudge skipped: `cswap auto --once` timed out");
            return None;
        }
    };
    let outcome = match output.status.code() {
        Some(0) => AutoOnceOutcome::Switched,
        Some(2) => AutoOnceOutcome::NoAction,
        Some(3) => AutoOnceOutcome::Blocked,
        _ => AutoOnceOutcome::Error,
    };
    let last_line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_string());
    crate::logging::info(&format!("cswap auto --once: {outcome:?} {last_line}"));
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn stub_cswap(dir: &std::path::Path, exit_code: u8) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(format!("cswap-stub-{exit_code}"));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n[ \"$1 $2 $3\" = \"auto --once --json\" ] || exit 99\necho '{{\"event\":\"stub\"}}'\nexit {exit_code}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_switch_once_maps_exit_codes() {
        let sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
        let temp = sandbox.external_dir();
        for (code, expected) in [
            (0, AutoOnceOutcome::Switched),
            (1, AutoOnceOutcome::Error),
            (2, AutoOnceOutcome::NoAction),
            (3, AutoOnceOutcome::Blocked),
        ] {
            let stub = stub_cswap(&temp, code);
            crate::env::set_var("JCODE_CSWAP_COMMAND", &stub);
            assert_eq!(auto_switch_once().await, Some(expected), "exit {code}");
        }
        crate::env::remove_var("JCODE_CSWAP_COMMAND");
    }

    #[tokio::test]
    async fn auto_switch_once_is_none_when_cswap_is_missing() {
        let _sandbox = crate::auth::test_sandbox::AuthTestSandbox::new().unwrap();
        crate::env::set_var("JCODE_CSWAP_COMMAND", "/nonexistent/cswap-not-installed");
        assert_eq!(auto_switch_once().await, None);
        crate::env::remove_var("JCODE_CSWAP_COMMAND");
    }
}
