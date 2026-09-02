//! Tests for following Claude Code's credential file as the live source of
//! truth: an external rewrite (cswap switch, `claude login`) must reach a
//! running provider on its next token fetch.

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

/// Set the file's mtime one second later than it currently is so a rewrite
/// within the same filesystem timestamp granularity still reads as a change.
fn bump_mtime(path: &std::path::Path) {
    let current = std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .expect("mtime");
    let bumped = current + std::time::Duration::from_secs(1);
    let file = std::fs::File::options()
        .write(true)
        .open(path)
        .expect("open for utime");
    file.set_modified(bumped).expect("set mtime");
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

#[tokio::test]
async fn cached_token_is_reused_while_credential_file_is_unchanged() {
    let (_sandbox, _path) = trusted_sandbox_with_file("token-a");
    let provider = AnthropicProvider::new();
    provider
        .set_credential_mode(AnthropicCredentialMode::OAuth)
        .expect("oauth mode");

    let (first, _) = provider
        .get_oauth_access_token()
        .await
        .expect("first fetch");
    let (second, _) = provider
        .get_oauth_access_token()
        .await
        .expect("second fetch");
    assert_eq!(first, "token-a");
    assert_eq!(second, "token-a");
    assert!(
        provider.credentials.read().await.is_some(),
        "the token must be cached between fetches"
    );
}

#[tokio::test]
async fn credential_file_rewrite_replaces_cached_token_on_next_fetch() {
    let (_sandbox, path) = trusted_sandbox_with_file("token-a");
    let provider = AnthropicProvider::new();
    provider
        .set_credential_mode(AnthropicCredentialMode::OAuth)
        .expect("oauth mode");

    let (first, _) = provider
        .get_oauth_access_token()
        .await
        .expect("first fetch");
    assert_eq!(first, "token-a");

    // An external switch: cswap rewrites the file with another account's
    // still-valid token. The cached token has not expired, so only the mtime
    // check can notice.
    write_credential_file(&path, "token-b", far_future_ms());
    bump_mtime(&path);

    let (second, _) = provider
        .get_oauth_access_token()
        .await
        .expect("second fetch");
    assert_eq!(second, "token-b", "a file flip must reach the next fetch");
    let cached = provider.credentials.read().await;
    let cached = cached.as_ref().expect("re-cached");
    assert_eq!(cached.access_token, "token-b");
    assert_eq!(
        cached.file_mtime,
        jcode_base::auth::claude::credential_file_mtime(),
        "the cache must be stamped with the new mtime"
    );
}
