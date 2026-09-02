//! Claude OAuth refresh-token grant and the single-flight coordination
//! around it (split out of `oauth.rs`).

use crate::auth::claude as claude_auth;
use crate::auth::oauth::{
    CLAUDE_TOKEN_TIMEOUT_SECS, OAuthTokens, claude, ensure_claude_inference_scope,
    parse_oauth_scopes, save_claude_tokens_for_account,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize)]
struct ClaudeRefreshTokenRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
}

#[derive(Deserialize)]
struct ClaudeRefreshTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    scope: Option<String>,
}

pub(crate) fn claude_refresh_error_is_invalid_scope(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("invalid_scope")
        || text.contains("requested scope is invalid")
        || text.contains("scope is invalid")
}

/// Lock retry jitter window, shrinkable in tests via `JCODE_CLAUDE_LOCK_RETRY_MS`.
fn lock_retry_delay_ms() -> (u64, u64) {
    if cfg!(test)
        && let Ok(value) = std::env::var("JCODE_CLAUDE_LOCK_RETRY_MS")
        && let Ok(ms) = value.parse::<u64>()
    {
        return (ms, ms);
    }
    crate::auth::claude::claude_code_locks::RETRY_DELAY_MS
}

/// Token endpoint, overridable in tests via `JCODE_CLAUDE_TOKEN_URL` so the
/// grant can be exercised against a local mock server.
fn claude_token_url() -> String {
    if cfg!(test)
        && let Ok(url) = std::env::var("JCODE_CLAUDE_TOKEN_URL")
    {
        return url;
    }
    claude::TOKEN_URL.to_string()
}

async fn send_claude_refresh_request(
    refresh_token: &str,
    scope: Option<&'static str>,
) -> Result<ClaudeRefreshTokenResponse> {
    let payload = ClaudeRefreshTokenRequest {
        grant_type: "refresh_token",
        refresh_token,
        client_id: claude::CLIENT_ID,
        scope,
    };

    let client = crate::provider::shared_http_client();
    let resp = client
        .post(claude_token_url())
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(CLAUDE_TOKEN_TIMEOUT_SECS))
        .json(&payload)
        .send()
        .await?;

    if !resp.status().is_success() {
        let text = resp.text().await?;
        let scope_label = scope.unwrap_or("<omitted>");
        anyhow::bail!(
            "Token refresh failed with scope '{}': {}",
            scope_label,
            text
        );
    }

    Ok(resp.json().await?)
}

/// The bare refresh-token grant. Persisting the result is the caller's job.
async fn refresh_claude_tokens_inner(refresh_token: &str) -> Result<OAuthTokens> {
    let scoped_result =
        send_claude_refresh_request(refresh_token, Some(claude::REFRESH_SCOPES)).await;
    let tokens = match scoped_result {
        Ok(tokens) => tokens,
        Err(err) if claude_refresh_error_is_invalid_scope(&err) => {
            crate::logging::warn(
                "Claude token refresh rejected Claude Code scopes; retrying without an explicit scope for legacy token compatibility",
            );
            match send_claude_refresh_request(refresh_token, None).await {
                Ok(tokens) => tokens,
                Err(fallback_err) => {
                    anyhow::bail!(
                        "Claude token refresh fallback without scope failed: {fallback_err:#}; scoped refresh error: {err:#}"
                    );
                }
            }
        }
        Err(err) => return Err(err),
    };

    let expires_at = chrono::Utc::now().timestamp_millis() + (tokens.expires_in * 1000);
    let scopes = parse_oauth_scopes(tokens.scope.as_deref());
    ensure_claude_inference_scope(&scopes, "token refresh")?;
    let oauth_tokens = OAuthTokens {
        access_token: tokens.access_token,
        refresh_token: tokens
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires_at,
        id_token: None,
        scopes,
    };

    Ok(oauth_tokens)
}

/// Run the grant and persist into jcode's own `auth.json` under `label`.
async fn refresh_claude_tokens_into_auth_json(
    refresh_token: &str,
    label: &str,
) -> Result<OAuthTokens> {
    let oauth_tokens = refresh_claude_tokens_inner(refresh_token).await?;
    save_claude_tokens_for_account(&oauth_tokens, label)?;
    Ok(oauth_tokens)
}

/// Refresh a token that lives in Claude Code's credential file, joining Claude
/// Code's own refresh protocol: hold both credential locks, re-read the file,
/// abort when another process (Claude Code, cswap) already replaced the
/// token, otherwise run the grant and write the result back atomically. The
/// file is the only store touched; nothing is written to `auth.json`.
pub(crate) async fn refresh_claude_tokens_in_credential_file(
    refresh_token: &str,
) -> Result<OAuthTokens> {
    let config_home = claude_auth::claude_code_config_home()?;
    let _locks = crate::auth::claude::claude_code_locks::acquire_credential_locks(
        &config_home,
        lock_retry_delay_ms(),
    )
    .await?;

    // Double-checked re-read under the lock, as Claude Code does.
    let current = claude_auth::load_trusted_claude_code_credentials()
        .ok_or_else(|| anyhow::anyhow!("Claude Code credential file vanished during refresh"))?;
    if current.refresh_token != refresh_token
        && crate::auth::refresh_coordinator::expiry_is_fresh(current.expires_at)
    {
        crate::logging::info(
            "Claude Code credential file was refreshed or switched by another process; using it as-is",
        );
        return Ok(OAuthTokens {
            access_token: current.access_token,
            refresh_token: current.refresh_token,
            expires_at: current.expires_at,
            id_token: None,
            scopes: current.scopes,
        });
    }

    let oauth_tokens = refresh_claude_tokens_inner(&current.refresh_token).await?;
    write_claude_code_credentials(&config_home.join(".credentials.json"), &oauth_tokens)?;
    crate::logging::info("Refreshed Claude Code credential file under its locks");
    Ok(oauth_tokens)
}

/// Refresh Claude OAuth tokens for the active (or primary) stored account.
pub async fn refresh_claude_tokens(refresh_token: &str) -> Result<OAuthTokens> {
    let label =
        claude_auth::active_account_label().unwrap_or_else(claude_auth::primary_account_label);
    refresh_claude_tokens_for_account(refresh_token, &label).await
}

/// Stored Claude tokens for `label`, expressed as [`OAuthTokens`].
fn stored_claude_tokens(label: &str) -> Option<OAuthTokens> {
    let Ok(accounts) = claude_auth::list_accounts() else {
        return None;
    };
    let account = accounts
        .into_iter()
        .find(|account| account.label == label)?;
    Some(OAuthTokens {
        access_token: account.access,
        refresh_token: account.refresh,
        expires_at: account.expires,
        id_token: None,
        scopes: account.scopes,
    })
}

/// Refresh Claude OAuth tokens for a specific account.
///
/// Serialized per account via the refresh coordinator: Anthropic rotates
/// refresh tokens, so two concurrent refreshes can otherwise persist a dead
/// refresh token and break the account.
pub async fn refresh_claude_tokens_for_account(
    refresh_token: &str,
    label: &str,
) -> Result<OAuthTokens> {
    // A token sourced from Claude Code's credential file is refreshed in
    // place under Claude Code's locks; writing it into auth.json would leave
    // the file one rotation stale and race Claude Code's own refresh.
    if claude_auth::credential_file_owns_refresh_token(refresh_token) {
        let result = crate::auth::refresh_coordinator::single_flight(
            "claude:credential-file".to_string(),
            || None::<OAuthTokens>,
            |_| false,
            |_| refresh_claude_tokens_in_credential_file(refresh_token),
        )
        .await;
        crate::auth::refresh_state::record_refresh_outcome("claude", refresh_token, &result);
        return result;
    }

    let observed_refresh = refresh_token.to_string();
    let label = label.to_string();
    let result = crate::auth::refresh_coordinator::single_flight(
        format!("claude:{label}"),
        {
            let label = label.clone();
            move || stored_claude_tokens(&label)
        },
        {
            let observed = observed_refresh.clone();
            move |stored: &OAuthTokens| {
                stored.refresh_token != observed
                    && crate::auth::refresh_coordinator::expiry_is_fresh(stored.expires_at)
            }
        },
        move |stored: Option<OAuthTokens>| async move {
            // Prefer the newest stored refresh token over the caller's
            // possibly stale observation.
            let token = stored
                .map(|tokens| tokens.refresh_token)
                .filter(|token| !token.is_empty())
                .unwrap_or(observed_refresh);
            refresh_claude_tokens_into_auth_json(&token, &label).await
        },
    )
    .await;

    // Shared recorder: permanent rejections become terminal, transient
    // failures stay retryable. Same policy for every provider.
    crate::auth::refresh_state::record_refresh_outcome("claude", refresh_token, &result);

    result
}

/// Write refreshed tokens back into Claude Code's credential file, preserving
/// every other key (`refreshTokenExpiresAt`, `rateLimitTier`, siblings of
/// `claudeAiOauth`). Atomic: temp file in the same directory, 0600, fsync,
/// rename. Callers must hold Claude Code's credential locks
/// ([`super::claude_code_locks`]).
fn write_claude_code_credentials(path: &std::path::Path, tokens: &OAuthTokens) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read credentials from {:?}", path))?;
    let mut doc: serde_json::Value = serde_json::from_str(&content)
        .with_context(|| format!("Could not parse Claude credentials from {:?}", path))?;
    let oauth = match doc.get_mut("claudeAiOauth") {
        Some(wrapped) => wrapped,
        None => &mut doc,
    };
    let Some(oauth) = oauth.as_object_mut() else {
        anyhow::bail!("Claude credentials in {:?} are not a JSON object", path);
    };
    oauth.insert("accessToken".into(), tokens.access_token.clone().into());
    oauth.insert("refreshToken".into(), tokens.refresh_token.clone().into());
    oauth.insert("expiresAt".into(), tokens.expires_at.into());
    if !tokens.scopes.is_empty() {
        oauth.insert("scopes".into(), tokens.scopes.clone().into());
    }

    let dir = path
        .parent()
        .context("Claude Code credential path has no parent directory")?;
    let mut temp = tempfile::Builder::new()
        .prefix(".credentials.json.")
        .suffix(".tmp")
        .tempfile_in(dir)
        .with_context(|| format!("Could not create temp file in {:?}", dir))?;
    jcode_core::fs::set_permissions_owner_only(temp.path())?;
    {
        use std::io::Write;
        let file = temp.as_file_mut();
        file.write_all(&serde_json::to_vec(&doc)?)?;
        file.sync_all()?;
    }
    temp.persist(path)
        .with_context(|| format!("Could not replace {:?}", path))?;
    Ok(())
}
