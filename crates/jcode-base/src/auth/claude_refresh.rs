//! Claude OAuth refresh-token grant and the single-flight coordination
//! around it (split out of `oauth.rs`).

use super::oauth::{
    CLAUDE_TOKEN_TIMEOUT_SECS, OAuthTokens, claude, ensure_claude_inference_scope,
    parse_oauth_scopes, save_claude_tokens_for_account,
};
use crate::auth::claude as claude_auth;
use anyhow::Result;
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

pub(super) fn claude_refresh_error_is_invalid_scope(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_ascii_lowercase();
    text.contains("invalid_scope")
        || text.contains("requested scope is invalid")
        || text.contains("scope is invalid")
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
        .post(claude::TOKEN_URL)
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

async fn refresh_claude_tokens_inner(
    refresh_token: &str,
    label: Option<&str>,
) -> Result<OAuthTokens> {
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

    let save_label = label.map(ToString::to_string).unwrap_or_else(|| {
        claude_auth::active_account_label().unwrap_or_else(claude_auth::primary_account_label)
    });
    save_claude_tokens_for_account(&oauth_tokens, &save_label)?;

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
    let account = claude_auth::list_accounts()
        .ok()?
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
            refresh_claude_tokens_inner(&token, Some(&label)).await
        },
    )
    .await;

    // Shared recorder: permanent rejections become terminal, transient
    // failures stay retryable. Same policy for every provider.
    crate::auth::refresh_state::record_refresh_outcome("claude", refresh_token, &result);

    result
}
