//! Refreshing a token that lives in Claude Code's credential file: under
//! Claude Code's locks, double-checked, written back atomically, and never
//! copied into jcode's `auth.json`.

use super::*;
use crate::auth::claude::CLAUDE_CODE_AUTH_SOURCE_ID;
use crate::auth::claude::claude_code_locks::{legacy_lock_dir, oauth_refresh_lock_dir};
use crate::auth::test_sandbox::AuthTestSandbox;
use std::os::unix::fs::PermissionsExt;

struct FileFixture {
    _sandbox: AuthTestSandbox,
    config_home: std::path::PathBuf,
    path: std::path::PathBuf,
}

fn trusted_file(refresh_token: &str, expires_at: i64) -> FileFixture {
    let sandbox = AuthTestSandbox::new().expect("auth sandbox");
    let config_home = sandbox.external_dir().join(".claude");
    let path = config_home.join(".credentials.json");
    std::fs::create_dir_all(&config_home).unwrap();
    std::fs::write(
        &path,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "old-access",
                "refreshToken": refresh_token,
                "expiresAt": expires_at,
                "refreshTokenExpiresAt": 4102444800000_i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max",
                "rateLimitTier": "default_claude_max_5x",
            },
            "otherTopLevel": {"keep": true},
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    crate::config::Config::allow_external_auth_source_for_path(CLAUDE_CODE_AUTH_SOURCE_ID, &path)
        .unwrap();
    crate::config::Config::invalidate_cache();
    FileFixture {
        _sandbox: sandbox,
        config_home,
        path,
    }
}

fn read_file(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[tokio::test]
async fn file_owned_token_is_refreshed_in_place_and_not_copied_into_auth_json() {
    let fixture = trusted_file("old-refresh", 1);
    let (port, handle) = mock_token_server(
        200,
        &serde_json::json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
            "scope": "user:inference user:profile",
        })
        .to_string(),
    )
    .await;
    let _url = EnvVarGuard::set(
        "JCODE_CLAUDE_TOKEN_URL",
        std::path::Path::new(&format!("http://127.0.0.1:{port}/v1/oauth/token")),
    );

    let tokens = refresh_claude_tokens_for_account("old-refresh", "claude-ignored")
        .await
        .expect("refresh succeeds");
    assert_eq!(tokens.access_token, "new-access");
    assert_eq!(tokens.refresh_token, "new-refresh");
    let (_method, _path, _headers, body) = handle.await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["refresh_token"], "old-refresh");

    // The file rotated in place and kept its unrelated keys.
    let doc = read_file(&fixture.path);
    let oauth = &doc["claudeAiOauth"];
    assert_eq!(oauth["accessToken"], "new-access");
    assert_eq!(oauth["refreshToken"], "new-refresh");
    assert_eq!(oauth["expiresAt"], tokens.expires_at);
    assert_eq!(oauth["refreshTokenExpiresAt"], 4102444800000_i64);
    assert_eq!(oauth["rateLimitTier"], "default_claude_max_5x");
    assert_eq!(oauth["subscriptionType"], "max");
    assert_eq!(doc["otherTopLevel"]["keep"], true);
    let mode = std::fs::metadata(&fixture.path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    // Nothing leaked into jcode's own store, and no lock was left behind.
    assert!(crate::auth::claude::list_accounts().unwrap().is_empty());
    assert!(!oauth_refresh_lock_dir(&fixture.config_home).exists());
    assert!(!legacy_lock_dir(&fixture.config_home).exists());
    assert!(
        std::fs::read_dir(&fixture.config_home)
            .unwrap()
            .all(|entry| entry.unwrap().file_name() == ".credentials.json"),
        "no temp files left behind"
    );
}

#[tokio::test]
async fn refresh_aborts_when_file_was_switched_under_the_lock() {
    // The caller observed `old-refresh`, but by the time the locks are held
    // the file carries a different, still-fresh token (cswap switched or
    // Claude Code refreshed). No grant must be sent, and the file's token
    // wins.
    let fixture = trusted_file("switched-refresh", far_future_ms());
    std::fs::write(
        &fixture.path,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "switched-access",
                "refreshToken": "switched-refresh",
                "expiresAt": far_future_ms(),
            }
        })
        .to_string(),
    )
    .unwrap();
    // Point the token URL at a closed port: any grant attempt fails loudly.
    let _url = EnvVarGuard::set(
        "JCODE_CLAUDE_TOKEN_URL",
        std::path::Path::new("http://127.0.0.1:1/v1/oauth/token"),
    );

    let tokens = crate::auth::claude::claude_refresh::refresh_claude_tokens_in_credential_file(
        "stale-observed",
    )
    .await
    .expect("uses the switched token without a grant");
    assert_eq!(tokens.access_token, "switched-access");
    assert_eq!(tokens.refresh_token, "switched-refresh");
    assert!(!oauth_refresh_lock_dir(&fixture.config_home).exists());
}

#[tokio::test]
async fn refresh_gives_up_when_claude_code_holds_the_lock() {
    let fixture = trusted_file("old-refresh", 1);
    // A live Claude Code refresh: fresh legacy lock dir.
    std::fs::create_dir_all(legacy_lock_dir(&fixture.config_home)).unwrap();
    let _url = EnvVarGuard::set(
        "JCODE_CLAUDE_TOKEN_URL",
        std::path::Path::new("http://127.0.0.1:1/v1/oauth/token"),
    );
    let _fast = EnvVarGuard::set("JCODE_CLAUDE_LOCK_RETRY_MS", std::path::Path::new("1"));

    let err = match refresh_claude_tokens_for_account("old-refresh", "claude-ignored").await {
        Ok(_) => panic!("must not refresh while Claude Code holds the lock"),
        Err(err) => err,
    };
    assert!(
        format!("{err:#}").contains("another process is refreshing"),
        "unexpected error: {err:#}"
    );
    assert_eq!(
        read_file(&fixture.path)["claudeAiOauth"]["refreshToken"],
        "old-refresh"
    );
    assert!(!oauth_refresh_lock_dir(&fixture.config_home).exists());
}

fn far_future_ms() -> i64 {
    chrono::Utc::now().timestamp_millis() + 3_600_000
}
