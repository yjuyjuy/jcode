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

/// Minimum spacing between reactive account switches for one provider, so a
/// burst of concurrent 429s (or a rapid retry loop) cannot thrash the fleet
/// across accounts. Short enough that a genuinely capped account still fails
/// over promptly on the next turn.
const REACTIVE_SWITCH_COOLDOWN: Duration = Duration::from_secs(20);

static LAST_REACTIVE_SWITCH: LazyLock<Mutex<std::collections::HashMap<&'static str, Instant>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// React to a live rate-limit (HTTP 429) from `provider` by switching the
/// active account to a sibling with headroom, if one exists.
///
/// Returns the newly-selected account label when a switch was applied, or
/// `None` when there is no better account, only one account exists, or the
/// per-provider reactive cooldown has not elapsed. On a switch it sets the
/// in-process active-account override (the same mechanism startup selection and
/// the countdown failover use) and marks the previous account transiently
/// unavailable so the usage-driven selection routes around it until its window
/// data refreshes; it never rewrites stored credentials.
///
/// This is the mid-turn complement to the poll-driven, between-turns selection:
/// it lets a capped primary fail over immediately, before the retry, instead of
/// waiting out the full retry-after delay or the next turn boundary. The caller
/// (the provider runtime retry loop) is responsible for re-fetching the token
/// for the new active account before retrying.
pub fn reactive_switch_on_rate_limit(provider: ActiveProvider) -> Option<String> {
    let provider_key = super::MultiProvider::provider_label(provider);

    // Cooldown gate: never switch more often than REACTIVE_SWITCH_COOLDOWN for
    // this provider, so concurrent sessions or a tight retry loop cannot ping
    // the fleet between accounts.
    {
        let mut last = LAST_REACTIVE_SWITCH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(prev) = last.get(provider_key)
            && prev.elapsed() < REACTIVE_SWITCH_COOLDOWN
        {
            return None;
        }
        // Reserve the slot up front so a racing caller does not also switch.
        last.insert(provider_key, Instant::now());
    }

    let probe = account_usage_probe(provider)?;
    if !probe.has_multiple_accounts() {
        return None;
    }
    let current_label = probe.current_label.clone();

    // Prefer the account with the most headroom that is not the current one and
    // is not itself errored/exhausted. Fall back to any non-current account
    // whose window data is missing (unknown headroom is better than a known
    // rate-limited account we just failed on).
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
    // arrives. The mark is keyed to the EXHAUSTED account label only, never the
    // provider as a whole: a drained account is not a dead provider, and the
    // healthy sibling we just selected must stay usable on the next failover
    // pass instead of being skipped into a cross-provider proposal.
    if provider == ActiveProvider::Claude {
        crate::provider::models::record_provider_unavailable_for_account_label(
            "claude",
            &current_label,
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

/// Whether an Anthropic account OTHER than the current one currently has usage
/// headroom (is not exhausted, not errored, and has multiple accounts).
///
/// Used by the TUI to decide whether a rate-limit / account failure should
/// suppress the interactive model-fallback (Ctrl+Y) offer: if another account
/// can serve the same model, the reactive account switch should handle it and
/// the user must never be nudged into a model downgrade for an account problem.
pub fn anthropic_has_alternate_account_with_headroom() -> bool {
    let Some(probe) = account_usage_probe(ActiveProvider::Claude) else {
        return false;
    };
    if !probe.has_multiple_accounts() {
        return false;
    }
    probe
        .accounts
        .iter()
        .filter(|account| account.label != probe.current_label)
        .any(|account| !account.exhausted && account.error.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_blocks_rapid_reactive_switches() {
        // Two switches within the cooldown window: the second must be gated.
        // Uses a throwaway provider key space by exercising the gate directly.
        let mut map = LAST_REACTIVE_SWITCH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert("test-key", Instant::now());
        let gated = map
            .get("test-key")
            .map(|prev| prev.elapsed() < REACTIVE_SWITCH_COOLDOWN)
            .unwrap_or(false);
        assert!(gated, "a fresh switch record must gate an immediate retry");
    }
}
