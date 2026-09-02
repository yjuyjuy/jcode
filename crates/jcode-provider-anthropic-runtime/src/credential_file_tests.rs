//! Tests for the cswap nudge: after a usage-limit 429 on a request served from
//! Claude Code's credential file, jcode asks cswap for one decision tick and
//! retries on whatever account it switched the file to.

use super::*;
use jcode_base::auth::claude::CLAUDE_CODE_AUTH_SOURCE_ID;
use jcode_base::auth::test_sandbox::AuthTestSandbox;

fn far_future_ms() -> i64 {
    chrono::Utc::now().timestamp_millis() + 3_600_000
}

fn credential_file_path(sandbox: &AuthTestSandbox) -> std::path::PathBuf {
    sandbox
        .external_dir()
        .join(".claude")
        .join(".credentials.json")
}

fn write_credential_file(path: &std::path::Path, access_token: &str, expires_at: i64) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        path,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": format!("{access_token}-refresh"),
                "expiresAt": expires_at,
                "scopes": ["user:inference", "user:profile"],
            }
        })
        .to_string(),
    )
    .expect("write credential file");
}

fn trusted_sandbox_with_file(access_token: &str) -> (AuthTestSandbox, std::path::PathBuf) {
    let sandbox = AuthTestSandbox::new().expect("auth sandbox");
    let path = credential_file_path(&sandbox);
    write_credential_file(&path, access_token, far_future_ms());
    jcode_base::config::Config::allow_external_auth_source_for_path(
        CLAUDE_CODE_AUTH_SOURCE_ID,
        &path,
    )
    .expect("trust credential file");
    jcode_base::config::Config::invalidate_cache();
    (sandbox, path)
}

/// A stand-in `cswap` whose `auto --once --json` rewrites the credential file
/// with `switched_token` and exits with `exit_code`, mirroring a real switch.
#[cfg(unix)]
fn stub_cswap(
    sandbox: &AuthTestSandbox,
    credential_path: &std::path::Path,
    switched_token: &str,
    exit_code: u8,
) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let stub = sandbox.external_dir().join(format!("cswap-{exit_code}"));
    let switched = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": switched_token,
            "refreshToken": format!("{switched_token}-refresh"),
            "expiresAt": far_future_ms(),
            "scopes": ["user:inference", "user:profile"],
        }
    });
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\n[ \"$1 $2 $3\" = \"auto --once --json\" ] || exit 99\ncat > '{}' <<'JSON'\n{}\nJSON\nexit {exit_code}\n",
            credential_path.display(),
            switched
        ),
    )
    .expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    stub
}

#[cfg(unix)]
#[tokio::test]
async fn rate_limit_nudge_retries_on_the_account_cswap_switched_to() {
    let (sandbox, path) = trusted_sandbox_with_file("token-a");
    let provider = AnthropicProvider::new();
    provider
        .set_credential_mode(AnthropicCredentialMode::OAuth)
        .expect("oauth mode");
    let (first, _) = provider
        .get_oauth_access_token()
        .await
        .expect("first fetch");
    assert_eq!(first, "token-a");

    let stub = stub_cswap(&sandbox, &path, "token-b", 0);
    jcode_base::env::set_var("JCODE_CSWAP_COMMAND", &stub);
    let nudged = nudge_cswap_after_rate_limit(Arc::clone(&provider.credentials)).await;
    jcode_base::env::remove_var("JCODE_CSWAP_COMMAND");

    assert_eq!(nudged.as_deref(), Some("token-b"));
    let cached = provider.credentials.read().await;
    let cached = cached.as_ref().expect("re-cached after switch");
    assert_eq!(cached.access_token, "token-b");
}

#[cfg(unix)]
#[tokio::test]
async fn rate_limit_nudge_is_a_no_op_when_cswap_declines() {
    let (sandbox, path) = trusted_sandbox_with_file("token-a");
    let provider = AnthropicProvider::new();
    provider
        .set_credential_mode(AnthropicCredentialMode::OAuth)
        .expect("oauth mode");
    provider
        .get_oauth_access_token()
        .await
        .expect("first fetch");

    // Exit 2 (no action) must leave the cache alone even though the stub
    // touched the file.
    let stub = stub_cswap(&sandbox, &path, "token-b", 2);
    jcode_base::env::set_var("JCODE_CSWAP_COMMAND", &stub);
    let declined = nudge_cswap_after_rate_limit(Arc::clone(&provider.credentials)).await;
    jcode_base::env::remove_var("JCODE_CSWAP_COMMAND");
    assert_eq!(declined, None);
    assert_eq!(
        provider
            .credentials
            .read()
            .await
            .as_ref()
            .map(|c| c.access_token.clone()),
        Some("token-a".to_string())
    );
}
