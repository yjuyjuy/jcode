//! Credential refresh helpers used by the streaming retry loop.
//!
//! These run inside the spawned retry task, which holds the shared credential
//! cache but not the provider itself, so they operate on the cache directly.

use super::CachedCredentials;
use anyhow::{Context, Result};
use jcode_base::auth;
use jcode_base::auth::oauth;
use std::sync::Arc;
use tokio::sync::RwLock;

pub(crate) async fn force_refresh_oauth_token(
    credentials: Arc<RwLock<Option<CachedCredentials>>>,
) -> Result<String> {
    let refresh_from_cache = {
        let cached = credentials.read().await;
        cached
            .as_ref()
            .map(|c| c.refresh_token.clone())
            .filter(|t| !t.is_empty())
    };

    let refresh_token = if let Some(token) = refresh_from_cache {
        token
    } else {
        let loaded = auth::claude::load_credentials()
            .context("Failed to load Claude credentials for forced refresh")?;
        if loaded.refresh_token.is_empty() {
            anyhow::bail!("No refresh token available in Claude credentials");
        }
        loaded.refresh_token
    };

    let active_label =
        auth::claude::active_account_label().unwrap_or_else(auth::claude::primary_account_label);
    let refreshed =
        match oauth::refresh_claude_tokens_for_account(&refresh_token, &active_label).await {
            Ok(refreshed) => refreshed,
            Err(err) => {
                anyhow::bail!("OAuth refresh endpoint rejected the refresh token: {err:#}");
            }
        };

    {
        let mut cached = credentials.write().await;
        *cached = Some(CachedCredentials {
            access_token: refreshed.access_token.clone(),
            refresh_token: refreshed.refresh_token,
            expires_at: refreshed.expires_at,
        });
    }

    Ok(refreshed.access_token)
}

/// After a usage-limit 429 on a request served from Claude Code's credential
/// file, ask cswap for one decision tick and, if it switched the file to
/// another account, load that account's token and return it so the retry loop
/// can go again immediately. jcode never picks the target: cswap owns the
/// file and the policy. `None` means "nothing changed, take the ordinary
/// retry path": the file is not the active source, cswap is missing, or it
/// chose not to switch.
pub(crate) async fn nudge_cswap_after_rate_limit(
    credentials: Arc<RwLock<Option<CachedCredentials>>>,
) -> Option<String> {
    // Only when the token that just got capped came from the file: the file
    // is trusted and its refresh token is the one we are holding.
    {
        let cached = credentials.read().await;
        let held = cached.as_ref().map(|c| c.refresh_token.as_str());
        let file = auth::claude::load_trusted_claude_code_credentials()?;
        if held.is_none_or(|held| held != file.refresh_token) {
            return None;
        }
    }
    if auth::cswap::auto_switch_once().await? != auth::cswap::AutoOnceOutcome::Switched {
        return None;
    }
    let loaded = match auth::claude::load_credentials() {
        Ok(loaded) => loaded,
        Err(err) => {
            jcode_base::logging::warn(&format!(
                "cswap switched the credential file but it could not be loaded: {err:#}"
            ));
            return None;
        }
    };
    let mut cached = credentials.write().await;
    *cached = Some(CachedCredentials {
        access_token: loaded.access_token.clone(),
        refresh_token: loaded.refresh_token,
        expires_at: loaded.expires_at,
    });
    Some(loaded.access_token)
}
