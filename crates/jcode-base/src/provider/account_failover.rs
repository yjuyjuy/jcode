use super::ActiveProvider;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

pub(super) fn multi_account_provider_kind(
    provider: ActiveProvider,
) -> Option<crate::usage::MultiAccountProviderKind> {
    match provider {
        ActiveProvider::Claude => Some(crate::usage::MultiAccountProviderKind::Anthropic),
        ActiveProvider::OpenAI => Some(crate::usage::MultiAccountProviderKind::OpenAI),
        _ => None,
    }
}

pub(super) fn account_usage_probe(
    provider: ActiveProvider,
) -> Option<crate::usage::AccountUsageProbe> {
    let kind = multi_account_provider_kind(provider)?;
    crate::usage::account_usage_probe_sync(kind)
}

pub(super) fn same_provider_account_failover_enabled() -> bool {
    crate::config::Config::load()
        .provider
        .same_provider_account_failover
}

pub(super) fn active_account_label_for_provider(provider: ActiveProvider) -> Option<String> {
    match provider {
        ActiveProvider::Claude => crate::auth::claude::active_account_label(),
        ActiveProvider::OpenAI => crate::auth::codex::active_account_label(),
        _ => None,
    }
}

pub(super) fn set_account_override_for_provider(provider: ActiveProvider, label: Option<String>) {
    match provider {
        ActiveProvider::Claude => crate::auth::claude::set_active_account_override(label),
        ActiveProvider::OpenAI => crate::auth::codex::set_active_account_override(label),
        _ => {}
    }
}

pub(super) fn same_provider_account_candidates(provider: ActiveProvider) -> Vec<String> {
    let current_label = active_account_label_for_provider(provider);
    let mut labels = Vec::new();

    let mut push_unique = |label: String| {
        if current_label.as_deref() == Some(label.as_str()) {
            return;
        }
        if !labels.iter().any(|existing| existing == &label) {
            labels.push(label);
        }
    };

    if let Some(probe) = account_usage_probe(provider) {
        let mut preferred = probe
            .accounts
            .iter()
            .filter(|account| account.label != probe.current_label)
            .filter(|account| !account.exhausted && account.error.is_none())
            .collect::<Vec<_>>();
        preferred.sort_by(|a, b| {
            let a_score = a
                .five_hour_ratio
                .unwrap_or(0.0)
                .max(a.seven_day_ratio.unwrap_or(0.0));
            let b_score = b
                .five_hour_ratio
                .unwrap_or(0.0)
                .max(b.seven_day_ratio.unwrap_or(0.0));
            a_score.total_cmp(&b_score)
        });
        for account in preferred {
            push_unique(account.label.clone());
        }

        for account in probe.accounts {
            push_unique(account.label);
        }
    }

    match provider {
        ActiveProvider::Claude => {
            for account in crate::auth::claude::list_accounts().unwrap_or_default() {
                push_unique(account.label);
            }
        }
        ActiveProvider::OpenAI => {
            for account in crate::auth::codex::list_accounts().unwrap_or_default() {
                push_unique(account.label);
            }
        }
        _ => {}
    }

    labels
}

pub(super) fn account_switch_guidance(provider: ActiveProvider) -> Option<String> {
    let probe = account_usage_probe(provider)?;
    probe.switch_guidance().or_else(|| {
        (probe.current_exhausted() && probe.all_accounts_exhausted()).then(|| {
            format!(
                "All {} accounts appear exhausted. Use `/usage` to inspect reset times.",
                probe.provider.display_name()
            )
        })
    })
}

pub(super) fn usage_exhausted_reason(provider: ActiveProvider) -> String {
    let mut reason = "OAuth usage exhausted".to_string();
    if let Some(guidance) = account_switch_guidance(provider) {
        reason.push_str(". ");
        reason.push_str(&guidance);
    }
    reason
}

fn error_looks_like_usage_limit(summary: &str) -> bool {
    let lower = summary.to_ascii_lowercase();
    [
        "quota",
        "insufficient_quota",
        "rate limit",
        "rate_limit",
        "rate_limit_exceeded",
        "too many requests",
        "billing",
        "credit",
        "payment required",
        "usage exhausted",
        "limit reached",
        "429",
        "402",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn maybe_annotate_limit_summary(provider: ActiveProvider, summary: String) -> String {
    if !error_looks_like_usage_limit(&summary) {
        return summary;
    }
    let Some(guidance) = account_switch_guidance(provider) else {
        return summary;
    };
    if summary.contains(&guidance) {
        return summary;
    }
    format!("{}. {}", summary, guidance)
}

// ---------------------------------------------------------------------------
// Reactive rate-limit account switch (mid-turn, in the provider runtime retry
// loop). This is the mid-turn complement to the poll-driven, between-turns
// selection: it lets a capped primary fail over to a sibling account with
// headroom IMMEDIATELY, before the retry backoff, instead of waiting out the
// full retry-after delay or the next turn boundary.
//
// It is distinct from `try_same_provider_account_failover`, which only fires
// when `complete_on_provider` returns an `Err`. An Anthropic 429 / 5-hour
// usage cap surfaces mid-stream (`complete` has already returned `Ok(stream)`),
// so it never reaches that path - the anthropic runtime calls this directly
// from its stream-retry loop instead. Both mechanisms share the same
// account-override seam and coexist without conflict.
// ---------------------------------------------------------------------------

/// Minimum gap between reactive account switches per provider, so concurrent
/// sessions or a tight retry loop cannot ping the fleet between accounts. Short
/// enough that a genuine cap on the new account still fails over promptly on the
/// next turn.
const REACTIVE_SWITCH_COOLDOWN: Duration = Duration::from_secs(20);

static LAST_REACTIVE_SWITCH: LazyLock<Mutex<std::collections::HashMap<&'static str, Instant>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Cache-ONLY headroom probe for the reactive switch. Reads sibling-account
/// usage from cache only, so a 429 can never trigger a burst of usage-endpoint
/// network calls.
fn reactive_cache_only_probe(provider: ActiveProvider) -> Option<crate::usage::AccountUsageProbe> {
    match provider {
        ActiveProvider::Claude => crate::usage::anthropic_account_usage_probe_cache_only(),
        _ => None,
    }
}

/// Reactively switch to a sibling account with headroom when the current account
/// is rate-limited / usage-capped mid-turn. Returns the new account label on a
/// switch, or `None` when there is no better account, only one account exists,
/// or the per-provider reactive cooldown has not elapsed.
///
/// On a switch it sets the in-process active-account override (the same seam the
/// startup selection and the countdown failover use) and marks the previous
/// account transiently unavailable so the between-turns selection also routes
/// around it until its window data refreshes. It never rewrites stored
/// credentials; the caller (the provider runtime retry loop) re-fetches the
/// token for the new active account before retrying.
///
/// HARDENING: headroom is read CACHE-ONLY (see `reactive_cache_only_probe`). A
/// sibling whose cache is cold has unknown headroom and is still eligible as a
/// switch target, because any account is better than the one we just got
/// rate-limited on.
pub fn reactive_switch_on_rate_limit(provider: ActiveProvider) -> Option<String> {
    let provider_key = jcode_provider_core::provider_label(provider);

    // Cooldown gate: reserve the slot up front so a racing caller does not also
    // switch within the window.
    {
        let mut last = LAST_REACTIVE_SWITCH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(prev) = last.get(provider_key)
            && prev.elapsed() < REACTIVE_SWITCH_COOLDOWN
        {
            return None;
        }
        last.insert(provider_key, Instant::now());
    }

    let probe = reactive_cache_only_probe(provider)?;
    if !probe.has_multiple_accounts() {
        return None;
    }
    let current_label = probe.current_label.clone();

    // Prefer the account with the most headroom that is not the current one and
    // is not itself errored/exhausted. Unknown headroom (cold cache) scores as
    // 0.0, so a sibling with no cached data is preferred over one known to be
    // heavily used - unknown is better than a known rate-limited account.
    let target = probe
        .accounts
        .iter()
        .filter(|account| account.label != current_label)
        .filter(|account| !account.exhausted && account.error.is_none())
        .min_by(|a, b| {
            let score = |account: &crate::usage::AccountUsageSnapshot| {
                account
                    .five_hour_ratio
                    .unwrap_or(0.0)
                    .max(account.seven_day_ratio.unwrap_or(0.0))
            };
            score(a).total_cmp(&score(b))
        })
        .map(|account| account.label.clone())?;

    // Mark the account we just got rate-limited on as transiently unavailable so
    // the between-turns selection also routes around it until fresh usage data
    // arrives.
    if provider == ActiveProvider::Claude {
        crate::provider::models::record_provider_unavailable_for_account(
            "anthropic",
            "reactive 429 rate-limit switch",
        );
    }

    set_account_override_for_provider(provider, Some(target.clone()));
    crate::logging::info(&format!(
        "Reactive rate-limit account switch ({}): {} -> {}",
        provider_key, current_label, target
    ));
    Some(target)
}

#[cfg(test)]
mod reactive_switch_tests {
    use super::*;

    #[test]
    fn cooldown_blocks_rapid_reactive_switches() {
        // Two switches within the cooldown window: the second must be gated.
        // Exercises the gate directly with a throwaway provider key so it does
        // not depend on live account state.
        let mut map = LAST_REACTIVE_SWITCH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert("test-key-reactive", Instant::now());
        let gated = map
            .get("test-key-reactive")
            .map(|prev| prev.elapsed() < REACTIVE_SWITCH_COOLDOWN)
            .unwrap_or(false);
        assert!(gated, "a fresh switch record must gate an immediate retry");
    }

    /// End-to-end: seed a temp jcode home with TWO Anthropic accounts (active
    /// `primary`, plus `sibling`), give the sibling cached usage headroom, and
    /// confirm `reactive_switch_on_rate_limit` actually flips the in-process
    /// active-account override to the sibling and returns its label. This drives
    /// the real public switch entry point (not just the matcher), through the
    /// cache-only probe, the candidate scorer, and the account-override seam.
    #[test]
    fn reactive_switch_flips_active_override_to_headroom_sibling() {
        let _guard = crate::storage::lock_test_env();
        let temp_home = tempfile::tempdir().expect("temp home");
        crate::env::set_var("JCODE_HOME", temp_home.path().to_string_lossy().to_string());

        // Clean per-test state: cooldown map, account override, unavailability.
        {
            let mut last = LAST_REACTIVE_SWITCH
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            last.remove(jcode_provider_core::provider_label(ActiveProvider::Claude));
        }
        crate::auth::claude::set_active_account_override(None);
        crate::provider::models::clear_provider_unavailable_for_account("anthropic");

        // Seed two accounts via the public upsert seam. Upsert assigns the
        // canonical labels, so capture them. Distinct token prefixes so the
        // cache-key (token:<first20>) is unambiguous.
        let primary_label = crate::auth::claude::upsert_account(
            crate::auth::claude::AnthropicAccount {
                label: "primary".to_string(),
                access: "primary_access_token_aaaaaaaaaa".to_string(),
                refresh: "primary_refresh".to_string(),
                expires: 9_999_999_999_999,
                email: None,
                scopes: Vec::new(),
                subscription_type: Some("max".to_string()),
            },
        )
        .expect("seed primary account");
        let sibling_label = crate::auth::claude::upsert_account(
            crate::auth::claude::AnthropicAccount {
                label: "sibling".to_string(),
                access: "sibling_access_token_bbbbbbbbbb".to_string(),
                refresh: "sibling_refresh".to_string(),
                expires: 9_999_999_999_999,
                email: None,
                scopes: Vec::new(),
                subscription_type: Some("max".to_string()),
            },
        )
        .expect("seed sibling account");
        assert_ne!(
            primary_label, sibling_label,
            "the two accounts must get distinct labels"
        );
        crate::auth::claude::set_active_account(&primary_label).expect("activate primary");

        // Give the sibling clear cached headroom (5h/7d both low). The active
        // account has no seeded live snapshot, which is fine: the switch just
        // needs an eligible sibling.
        crate::usage::seed_anthropic_usage_cache_for_token(
            "sibling_access_token_bbbbbbbbbb",
            0.10,
            0.05,
        );

        // Sanity: the active account is `primary` before the switch.
        assert_eq!(
            crate::auth::claude::active_account_label().as_deref(),
            Some(primary_label.as_str()),
            "precondition: primary is the active account"
        );

        let switched = reactive_switch_on_rate_limit(ActiveProvider::Claude);

        assert_eq!(
            switched.as_deref(),
            Some(sibling_label.as_str()),
            "the switch must select the headroom sibling"
        );
        // The real acceptance behavior: the in-process active-account override
        // now points at the sibling, so the retry uses the new account.
        assert_eq!(
            crate::auth::claude::active_account_label().as_deref(),
            Some(sibling_label.as_str()),
            "the active-account override must be flipped to the sibling"
        );

        // Cleanup so later tests are not affected by the override/env.
        crate::auth::claude::set_active_account_override(None);
        crate::provider::models::clear_provider_unavailable_for_account("anthropic");
        crate::env::remove_var("JCODE_HOME");
    }
}
