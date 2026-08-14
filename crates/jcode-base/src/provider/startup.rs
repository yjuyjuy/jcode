use super::*;

/// Minimum seconds between poll-driven account re-evaluations.
///
/// Defaults to 60 seconds; overridable via `JCODE_ACCOUNT_REEVAL_INTERVAL_SECS`.
/// `0` disables poll-driven re-evaluation entirely. A malformed value falls back
/// to the default.
fn account_reeval_interval_secs() -> u64 {
    parse_account_reeval_interval(
        std::env::var("JCODE_ACCOUNT_REEVAL_INTERVAL_SECS")
            .ok()
            .as_deref(),
    )
}

/// Default poll-driven re-evaluation interval, in seconds.
const DEFAULT_ACCOUNT_REEVAL_INTERVAL_SECS: u64 = 60;

/// Pure parse of the re-eval interval override. `None` (unset) and a malformed
/// value both fall back to [`DEFAULT_ACCOUNT_REEVAL_INTERVAL_SECS`]; a valid
/// non-negative integer (including `0`, which disables) is honored.
fn parse_account_reeval_interval(raw: Option<&str>) -> u64 {
    match raw {
        Some(value) => value
            .trim()
            .parse::<u64>()
            .unwrap_or(DEFAULT_ACCOUNT_REEVAL_INTERVAL_SECS),
        None => DEFAULT_ACCOUNT_REEVAL_INTERVAL_SECS,
    }
}

#[cfg(test)]
mod reeval_interval_tests {
    use super::{DEFAULT_ACCOUNT_REEVAL_INTERVAL_SECS, parse_account_reeval_interval};

    #[test]
    fn unset_uses_default() {
        assert_eq!(
            parse_account_reeval_interval(None),
            DEFAULT_ACCOUNT_REEVAL_INTERVAL_SECS
        );
    }

    #[test]
    fn malformed_uses_default() {
        assert_eq!(
            parse_account_reeval_interval(Some("not-a-number")),
            DEFAULT_ACCOUNT_REEVAL_INTERVAL_SECS
        );
    }

    #[test]
    fn zero_disables() {
        assert_eq!(parse_account_reeval_interval(Some("0")), 0);
    }

    #[test]
    fn valid_value_is_honored() {
        assert_eq!(parse_account_reeval_interval(Some("  120  ")), 120);
    }
}

impl MultiProvider {
    pub(super) fn spawn_post_auth_model_refresh(
        &self,
        provider: Arc<dyn Provider>,
        provider_label: &'static str,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            crate::logging::auth_event(
                "post_auth_model_refresh_skipped",
                provider_label,
                &[("reason", "no_tokio_runtime")],
            );
            return;
        };

        self.post_auth_refreshes_pending
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let pending = Arc::clone(&self.post_auth_refreshes_pending);

        handle.spawn(async move {
            struct PendingGuard(Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for PendingGuard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                }
            }
            let _pending_guard = PendingGuard(pending);
            let refresh_started = std::time::Instant::now();
            crate::logging::auth_event("post_auth_model_refresh_started", provider_label, &[]);
            provider.invalidate_credentials().await;
            match provider.prefetch_models().await {
                Ok(()) => {
                    let duration_ms = refresh_started.elapsed().as_millis().to_string();
                    crate::logging::auth_event(
                        "post_auth_model_refresh_completed",
                        provider_label,
                        &[("duration_ms", duration_ms.as_str())],
                    );
                    crate::bus::Bus::global().publish_models_updated();
                }
                Err(err) => {
                    let reason = err.to_string();
                    let duration_ms = refresh_started.elapsed().as_millis().to_string();
                    crate::logging::auth_event(
                        "post_auth_model_refresh_failed",
                        provider_label,
                        &[
                            ("reason", reason.as_str()),
                            ("duration_ms", duration_ms.as_str()),
                        ],
                    );
                    crate::logging::info(&format!(
                        "Failed to refresh {} models after auth change: {}",
                        provider_label, err
                    ));
                }
            }
        });
    }

    pub(super) async fn invalidate_provider_credentials_for_account_switch(
        &self,
        provider: ActiveProvider,
    ) {
        match provider {
            ActiveProvider::Claude => {
                if let Some(anthropic) = self.anthropic_provider() {
                    anthropic.invalidate_credentials().await;
                }
                if let Some(claude) = self.claude_provider() {
                    claude.invalidate_credentials().await;
                }
            }
            ActiveProvider::OpenAI => {
                if let Some(openai) = self.openai_provider() {
                    openai.invalidate_credentials().await;
                }
            }
            _ => {}
        }
    }

    pub(super) fn new_with_auth_status(auth_status: auth::AuthStatus) -> Self {
        let provider_init_start = std::time::Instant::now();
        let cfg = crate::config::config();
        let provider_state = ProviderState::from_parts(cfg, &auth_status);
        let mut default_named_provider_profile: Option<String> = None;
        if std::env::var_os("JCODE_PROVIDER_PROFILE_ACTIVE").is_none()
            && std::env::var_os("JCODE_NAMED_PROVIDER_PROFILE").is_none()
            && let Some(pref) = provider_state.default_provider_key()
        {
            if let Some(profile) =
                crate::provider_catalog::resolve_openai_compatible_profile_selection(pref)
            {
                crate::provider_catalog::apply_openai_compatible_profile_env(Some(profile));
            } else if cfg.providers.contains_key(pref) {
                match crate::provider_catalog::apply_named_provider_profile_env_from_config(
                    pref, cfg,
                ) {
                    Ok(profile_name) => {
                        crate::env::set_var("JCODE_PROVIDER_PROFILE_NAME", &profile_name);
                        crate::env::set_var("JCODE_PROVIDER_PROFILE_ACTIVE", "1");
                        default_named_provider_profile = Some(profile_name);
                    }
                    Err(err) => crate::logging::warn(&format!(
                        "Failed to apply default provider profile '{}': {}",
                        pref, err
                    )),
                }
            }
        }

        let has_claude_creds =
            auth::claude::load_credentials().is_ok() || anthropic::has_anthropic_api_key();
        let has_openai_creds = auth::codex::load_credentials().is_ok();
        let has_copilot_api = provider_state.auth_status().copilot_has_api_token;
        let has_antigravity_creds = auth::antigravity::load_tokens().is_ok();
        let has_gemini_creds = auth::gemini::load_tokens().is_ok() || auth::gemini::has_api_key();
        let has_cursor_creds = provider_state
            .auth_status()
            .assessment_for_provider(crate::provider_catalog::CURSOR_LOGIN_PROVIDER)
            .is_available();
        let has_bedrock_creds = bedrock::BedrockProvider::has_credentials();
        let has_openrouter_creds = openrouter::has_credentials();

        let use_claude_cli = std::env::var("JCODE_USE_CLAUDE_CLI")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if use_claude_cli {
            crate::logging::warn(
                "JCODE_USE_CLAUDE_CLI is deprecated and will be removed. Direct Anthropic API transport is the default.",
            );
        }

        let claude = if has_claude_creds && use_claude_cli {
            crate::logging::info(
                "Using deprecated Claude CLI provider (forced by JCODE_USE_CLAUDE_CLI=1)",
            );
            external::instantiate_expected_external_provider(external::CLAUDE_CLI_RUNTIME)
        } else {
            None
        };

        let anthropic = if has_claude_creds && !use_claude_cli {
            external::instantiate_expected_external_provider(external::ANTHROPIC_RUNTIME)
        } else {
            None
        };

        let openai = if has_openai_creds {
            external::instantiate_expected_external_provider(external::OPENAI_RUNTIME)
        } else {
            None
        };

        let copilot_api = if has_copilot_api {
            // The composition-root factory handles construction, tier-detection
            // scheduling (eager vs non-interactive deferral), and init-done
            // signaling; None means credentials were missing or invalid.
            let copilot_init_start = std::time::Instant::now();
            let provider =
                external::instantiate_expected_external_provider(external::COPILOT_RUNTIME);
            match &provider {
                Some(_) => crate::logging::info(&format!(
                    "Copilot API provider initialized (direct API) in {}ms",
                    copilot_init_start.elapsed().as_millis()
                )),
                None => crate::logging::info("Failed to initialize Copilot API (no credentials)"),
            }
            provider
        } else {
            None
        };

        let antigravity_provider = if has_antigravity_creds {
            external::instantiate_expected_external_provider(external::ANTIGRAVITY_RUNTIME)
        } else {
            None
        };

        let gemini_provider = if has_gemini_creds {
            external::instantiate_expected_external_provider(external::GEMINI_RUNTIME)
        } else {
            None
        };

        let cursor_provider = if has_cursor_creds {
            external::instantiate_expected_external_provider(external::CURSOR_RUNTIME)
        } else {
            None
        };

        let bedrock_provider = if has_bedrock_creds {
            Some(Arc::new(bedrock::BedrockProvider::new()))
        } else {
            None
        };

        let openrouter = if has_openrouter_creds {
            let named_profile = std::env::var("JCODE_NAMED_PROVIDER_PROFILE")
                .ok()
                .or_else(|| default_named_provider_profile.clone());
            let spec = named_profile
                .as_deref()
                .and_then(|profile_name| {
                    cfg.providers.get(profile_name).map(|profile| {
                        external::OpenRouterRuntimeSpec::NamedProfile {
                            name: profile_name.to_string(),
                            config: profile.clone(),
                        }
                    })
                })
                .unwrap_or(external::OpenRouterRuntimeSpec::Default);
            match external::instantiate_openrouter_runtime(spec) {
                Ok(p) => Some(p),
                Err(e) => {
                    crate::logging::info(&format!("Failed to initialize OpenRouter: {}", e));
                    None
                }
            }
        } else {
            None
        };

        let copilot_premium_zero = matches!(
            std::env::var("JCODE_COPILOT_PREMIUM").ok().as_deref(),
            Some("0")
        );
        let availability = ProviderAvailability {
            openai: openai.is_some(),
            claude: claude.is_some() || anthropic.is_some(),
            copilot: copilot_api.is_some(),
            antigravity: antigravity_provider.is_some(),
            gemini: gemini_provider.is_some(),
            cursor: cursor_provider.is_some(),
            bedrock: bedrock_provider.is_some(),
            openrouter: openrouter.is_some(),
            copilot_premium_zero,
        };
        let mut active = Self::auto_default_provider(availability);

        if copilot_premium_zero && matches!(active, ActiveProvider::Copilot) {
            crate::logging::info(
                "Copilot premium mode is Zero (free requests) - defaulting to Copilot provider",
            );
        }

        let initial_provider = Self::initial_provider_from_env();
        if let Some(initial) = initial_provider {
            active = initial;
            let is_configured = availability.is_configured(initial);
            if is_configured {
                let display = if matches!(initial, ActiveProvider::OpenRouter) {
                    crate::provider_catalog::active_openai_compatible_display_name()
                        .unwrap_or_else(|| Self::provider_key(initial).to_string())
                } else {
                    Self::provider_key(initial).to_string()
                };
                crate::logging::info(&format!(
                    "Using initial provider '{}' from CLI/environment",
                    display
                ));
            } else {
                crate::logging::warn(&format!(
                    "Initial provider '{}' is not configured; requests will fail until credentials are available or another model is selected",
                    Self::provider_key(initial)
                ));
            }
        } else if let Some(pref) = provider_state.default_provider_key() {
            if let Some(selection) = provider_state.default_provider_selection() {
                let preferred = selection.active_provider();
                let is_configured = provider_state
                    .preferred_provider_is_configured(availability)
                    .unwrap_or(false);
                if is_configured {
                    active = preferred;
                    crate::logging::info(&format!(
                        "Using preferred provider '{}' from config via {}",
                        pref,
                        provider_state
                            .preferred_provider_display_label()
                            .unwrap_or_else(|| selection.display_label())
                    ));
                } else {
                    crate::logging::warn(&format!(
                        "Preferred provider '{}' is not configured, using auto-detected default",
                        pref
                    ));
                }
            } else {
                crate::logging::warn(&format!(
                    "Unknown default_provider '{}' in config (expected: claude|openai|copilot|antigravity|gemini|cursor|bedrock|openrouter or an OpenAI-compatible profile such as deepseek|comtegra|zai|openai-compatible)",
                    pref
                ));
            }
        }

        let result = Self {
            claude: RwLock::new(claude),
            anthropic: RwLock::new(anthropic),
            openai: RwLock::new(openai),
            copilot_api: RwLock::new(copilot_api),
            antigravity: RwLock::new(antigravity_provider),
            gemini: RwLock::new(gemini_provider),
            cursor: RwLock::new(cursor_provider),
            bedrock: RwLock::new(bedrock_provider),
            openrouter: RwLock::new(openrouter),
            openai_compatible_profiles: RwLock::new(HashMap::new()),
            active_openai_compatible_profile: RwLock::new(None),
            active: RwLock::new(active),
            use_claude_cli,
            startup_notices: RwLock::new(Vec::new()),
            initial_provider,
            routes_memo: Mutex::new(None),
            post_auth_refreshes_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };

        if let Some(model) = provider_state.default_model() {
            if let Err(e) =
                result.set_config_default_model(model, provider_state.default_provider_key())
            {
                crate::logging::warn(&format!(
                    "Failed to apply default_model '{}' from config: {}",
                    model, e
                ));
            } else {
                crate::logging::info(&format!("Applied default model '{}' from config", model));
            }
        }

        result.spawn_anthropic_catalog_refresh_if_needed();
        result.spawn_openai_catalog_refresh_if_needed();
        result.auto_select_active_multi_account();
        crate::logging::info(&format!(
            "[TIMING] provider_init: claude={}, anthropic={}, openai={}, copilot={}, antigravity={}, gemini={}, cursor={}, bedrock={}, openrouter={}, total={}ms",
            result
                .claude
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            result
                .anthropic
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            result
                .openai
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            result
                .copilot_api
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            result
                .antigravity
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            result
                .gemini
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            result
                .cursor
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            result
                .bedrock
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            result
                .openrouter
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_some(),
            provider_init_start.elapsed().as_millis()
        ));
        result
    }

    pub(super) fn spawn_openai_catalog_refresh_if_needed(&self) {
        let Some(provider) = self.openai_provider() else {
            return;
        };
        if !begin_openai_model_catalog_refresh() {
            return;
        }

        tokio::spawn(async move {
            if let Err(err) = provider.prefetch_models().await {
                crate::logging::info(&format!(
                    "Failed to refresh OpenAI model catalog from provider bootstrap: {}",
                    err
                ));
            }
            finish_openai_model_catalog_refresh();
        });
    }

    pub(super) fn spawn_anthropic_catalog_refresh_if_needed(&self) {
        let provider: Arc<dyn Provider> = if let Some(anthropic) = self.anthropic_provider() {
            anthropic
        } else if let Some(claude) = self.claude_provider() {
            claude
        } else {
            return;
        };

        let Some(scope) = begin_anthropic_model_catalog_refresh() else {
            return;
        };

        tokio::spawn(async move {
            if let Err(err) = provider.prefetch_models().await {
                crate::logging::info(&format!(
                    "Failed to refresh Anthropic model catalog from provider bootstrap: {}",
                    err
                ));
            }
            finish_anthropic_model_catalog_refresh_for_scope(&scope);
        });
    }

    /// Create a new MultiProvider, detecting available credentials
    pub fn new() -> Self {
        Self::new_with_auth_status(auth::AuthStatus::check())
    }

    /// Create a startup-optimized MultiProvider that avoids expensive auth probes.
    pub fn new_fast() -> Self {
        Self::new_with_auth_status(auth::AuthStatus::check_fast())
    }

    pub fn from_auth_status(auth_status: auth::AuthStatus) -> Self {
        Self::new_with_auth_status(auth_status)
    }

    /// Create with explicit initial provider preference
    pub fn with_preference(prefer_openai: bool) -> Self {
        let provider = Self::new();
        if provider.initial_provider.is_none()
            && prefer_openai
            && provider.openai_provider().is_some()
        {
            *provider
                .active
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = ActiveProvider::OpenAI;
        }
        provider
    }

    pub fn with_preference_fast(prefer_openai: bool) -> Self {
        let provider = Self::new_fast();
        if provider.initial_provider.is_none()
            && prefer_openai
            && provider.openai_provider().is_some()
        {
            *provider
                .active
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = ActiveProvider::OpenAI;
        }
        provider
    }

    pub(super) fn active_provider(&self) -> ActiveProvider {
        *self
            .active
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn auto_select_active_multi_account(&self) {
        self.auto_select_multi_account_for_provider(self.active_provider());
    }

    /// Poll-driven account re-evaluation, safe to call at every turn boundary.
    ///
    /// Runs the same account-selection logic as [`auto_select_active_multi_account`]
    /// (so it honors the configured strategy: pace, priority, or exhaustion-only),
    /// but coarsely time-gates the expensive per-account usage probe so calling it
    /// once per turn never adds per-turn latency. This is what makes a priority
    /// strategy's "return on reset" and "fall back on cap", and pace balancing's
    /// ongoing rebalance, actually fire AFTER startup instead of only at provider
    /// construction and explicit provider switch.
    ///
    /// The re-evaluation interval defaults to 60 seconds and is overridable via
    /// `JCODE_ACCOUNT_REEVAL_INTERVAL_SECS`. A value of `0` disables the poll-driven
    /// re-evaluation entirely (falling back to construction-time and explicit-switch
    /// selection only).
    pub fn reevaluate_account_selection(&self) {
        use std::sync::{LazyLock, Mutex};

        let interval_secs = account_reeval_interval_secs();
        if interval_secs == 0 {
            return;
        }

        static LAST_REEVAL: LazyLock<Mutex<Option<f64>>> = LazyLock::new(|| Mutex::new(None));
        let now = chrono::Utc::now().timestamp() as f64;
        {
            let mut last = LAST_REEVAL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(prev) = *last
                && now - prev < interval_secs as f64
            {
                return;
            }
            *last = Some(now);
        }

        self.auto_select_active_multi_account();
    }

    /// Backward-compatible wrapper for the Anthropic-specific startup rotation entrypoint.
    pub fn auto_select_anthropic_account(&self) {
        self.auto_select_multi_account_for_provider(ActiveProvider::Claude);
    }

    pub fn auto_select_openai_account(&self) {
        self.auto_select_multi_account_for_provider(ActiveProvider::OpenAI);
    }

    pub(super) fn auto_select_multi_account_for_provider(&self, provider: ActiveProvider) {
        if self.active_provider() != provider {
            return;
        }
        if !self.provider_is_configured(provider) {
            return;
        }
        if provider == ActiveProvider::OpenAI {
            return;
        }

        let Some(probe) = account_usage_probe(provider) else {
            return;
        };
        if !probe.has_multiple_accounts() {
            return;
        }
        if !probe.current_exhausted() {
            // Not exhausted: the exhaustion-only failover below has nothing to
            // do. Try the configured same-provider rebalancing strategy instead
            // (pace-aware consume-first, ranked priority, or nothing), which can
            // proactively move the active account before it is fully exhausted.
            self.maybe_rebalance(provider, &probe);
            return;
        }

        let provider_name = probe.provider.display_name();
        if let Some(alternative) = probe.best_available_alternative() {
            crate::logging::info(&format!(
                "{} account '{}' is exhausted, switching to '{}' ({})",
                provider_name,
                probe.current_label,
                alternative.label,
                alternative.summary()
            ));

            match provider {
                ActiveProvider::Claude => {
                    crate::auth::claude::set_active_account_override(Some(
                        alternative.label.clone(),
                    ));
                    clear_all_provider_unavailability_for_account();
                    clear_all_model_unavailability_for_account();
                    if let Some(anthropic) = self.anthropic_provider() {
                        tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current()
                                .block_on(anthropic.invalidate_credentials())
                        });
                    }
                }
                ActiveProvider::OpenAI => {
                    crate::auth::codex::set_active_account_override(Some(
                        alternative.label.clone(),
                    ));
                    clear_all_provider_unavailability_for_account();
                    clear_all_model_unavailability_for_account();
                    if let Some(openai) = self.openai_provider() {
                        tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current()
                                .block_on(openai.invalidate_credentials())
                        });
                    }
                }
                _ => return,
            }

            let notice = format!(
                "⚡ Auto-switched {} account: **{}** -> **{}** (previous account exhausted)",
                provider_name, probe.current_label, alternative.label
            );
            self.startup_notices
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(notice);
            return;
        }

        if probe.all_accounts_exhausted() {
            crate::logging::info(&format!("All {} accounts are exhausted", provider_name));
            let notice = format!(
                "⚠ All {} accounts exhausted - will fall back to other providers if available",
                provider_name
            );
            self.startup_notices
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(notice);
        }
    }

    /// Check if Anthropic OAuth usage is exhausted (both 5hr and 7d at 100%)
    pub(super) fn is_claude_usage_exhausted(&self) -> bool {
        if !self.has_claude_runtime() {
            return false;
        }

        let usage = crate::usage::get_sync();
        usage.five_hour >= 0.99 && usage.seven_day >= 0.99
    }

    /// Same-provider rebalancing for a not-yet-exhausted provider, dispatched on
    /// the configured [`account_selection_strategy`]. `Pace` (the shipped
    /// default) keeps the consume-first balancing; `Priority` prefers the
    /// highest-ranked live account; `ExhaustionOnly` does nothing here and leaves
    /// the exhaustion-only failover path as the only mover.
    ///
    /// [`account_selection_strategy`]: jcode_config_types::ProviderConfig::account_selection_strategy
    fn maybe_rebalance(&self, provider: ActiveProvider, probe: &crate::usage::AccountUsageProbe) {
        use jcode_config_types::AccountSelectionStrategy;
        match crate::config::Config::load()
            .provider
            .account_selection_strategy
        {
            AccountSelectionStrategy::Pace => self.maybe_pace_balance(provider, probe),
            AccountSelectionStrategy::Priority => self.maybe_priority_select(provider, probe),
            AccountSelectionStrategy::ExhaustionOnly => {}
        }
    }

    /// Ranked-priority same-provider selection for a not-yet-exhausted provider.
    ///
    /// Asks the priority model (see [`crate::usage::pace::select_priority_target`])
    /// for the highest-ranked live account, keyed on stable identity (email, then
    /// label) from `[provider].account_priority`. A `Switch` or `Failover` sets
    /// the active-account override through the same [`Self::apply_account_override`]
    /// path the pace balancer and exhaustion failover use, so it never touches
    /// credentials beyond that override/invalidate path. An empty priority list
    /// leaves nothing to rank, so the selector simply stays put.
    fn maybe_priority_select(
        &self,
        provider: ActiveProvider,
        probe: &crate::usage::AccountUsageProbe,
    ) {
        use std::sync::{LazyLock, Mutex};

        // Only Anthropic exposes the per-account 5h/7d window data the priority
        // thresholds need; OpenAI is handled by the exhaustion path only for now.
        if provider != ActiveProvider::Claude {
            return;
        }

        let priority_order = crate::config::Config::load()
            .provider
            .account_priority
            .clone();
        if priority_order.is_empty() {
            // Nothing to rank: leave the exhaustion-only failover as the mover.
            return;
        }

        static LAST_SWITCH: LazyLock<Mutex<std::collections::HashMap<&'static str, f64>>> =
            LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
        let provider_key = Self::provider_label(provider);
        let now = chrono::Utc::now();
        let cfg = crate::usage::BalanceConfig::default();
        let state = crate::usage::BalanceState {
            last_switch_epoch: LAST_SWITCH
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(provider_key)
                .copied(),
        };

        let decision = probe.priority_decision(&priority_order, cfg, state, now);
        let (to, human_reason) = match &decision {
            crate::usage::BalanceDecision::Switch { to, reason } => {
                let human = match *reason {
                    "priority-return" => "higher-priority account available again",
                    "priority-cap" => "current account capped",
                    _ => "priority rebalance",
                };
                (to.clone(), human)
            }
            crate::usage::BalanceDecision::Failover { to } => {
                (to.clone(), "current account exhausted")
            }
            // Stay / Blocked / AllExhausted: nothing to do this round.
            _ => return,
        };

        crate::logging::info(&format!(
            "{} priority selection: moving {} -> {} ({})",
            probe.provider.display_name(),
            probe.current_label,
            to,
            human_reason
        ));
        self.apply_account_override(provider, &to);
        LAST_SWITCH
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(provider_key, now.timestamp() as f64);

        let glyph = if human_reason == "higher-priority account available again" {
            "↩"
        } else {
            "⤵"
        };
        let notice = format!(
            "{} Priority-switched {} account: **{}** -> **{}** ({})",
            glyph,
            probe.provider.display_name(),
            probe.current_label,
            to,
            human_reason
        );
        self.startup_notices
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(notice);
    }

    /// Pace-aware same-pace balancing for a not-yet-exhausted provider.
    ///
    /// Runs only when `[provider].pace_aware_account_balancing` is on. It asks
    /// the pace model for a same-pace balancing decision (consume the
    /// soonest-resetting weekly quota first, with hysteresis + a cooldown so it
    /// never flip-flops); a `Switch` sets the active-account override exactly as
    /// the exhaustion failover does. When no switch is warranted it considers
    /// priming an unopened 5-hour window on a peer, but only when more capacity
    /// will be needed soon (see [`crate::usage::should_prime`]).
    ///
    /// This never touches credentials beyond the same override/invalidate path
    /// the manual `/account switch` and exhaustion failover already use, and
    /// priming sends only a single minimal inference request.
    fn maybe_pace_balance(
        &self,
        provider: ActiveProvider,
        probe: &crate::usage::AccountUsageProbe,
    ) {
        use std::sync::{LazyLock, Mutex};

        if !crate::config::Config::load()
            .provider
            .pace_aware_account_balancing
        {
            return;
        }
        // Only Anthropic exposes the per-account 5h/7d window data the pace math
        // needs; OpenAI is handled by the exhaustion path only for now.
        if provider != ActiveProvider::Claude {
            return;
        }

        // Durable-enough cooldown for this process, keyed by provider label.
        static LAST_SWITCH: LazyLock<Mutex<std::collections::HashMap<&'static str, f64>>> =
            LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
        let provider_key = Self::provider_label(provider);
        let now = chrono::Utc::now();
        let cfg = crate::usage::BalanceConfig::default();
        let state = crate::usage::BalanceState {
            last_switch_epoch: LAST_SWITCH
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(provider_key)
                .copied(),
        };

        match probe.balanced_decision(cfg, state, now) {
            crate::usage::BalanceDecision::Switch { to, reason } => {
                crate::logging::info(&format!(
                    "{} pace balancing ({}): moving {} -> {}",
                    probe.provider.display_name(),
                    reason,
                    probe.current_label,
                    to
                ));
                self.apply_account_override(provider, &to);
                LAST_SWITCH
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(provider_key, now.timestamp() as f64);
                let notice = format!(
                    "⚡ Pace-balanced {} account: **{}** -> **{}** (consuming soonest-resetting quota first)",
                    probe.provider.display_name(),
                    probe.current_label,
                    to
                );
                self.startup_notices
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(notice);
            }
            crate::usage::BalanceDecision::Stay(_) | crate::usage::BalanceDecision::Blocked(_) => {
                // No switch warranted. Consider priming an unopened 5h window so
                // it is already counting down when the fleet needs it.
                if let Some(target) = probe.prime_candidate(cfg, now, 10.0) {
                    self.prime_account(provider, probe, &target);
                }
            }
            // Failover / AllExhausted only arise on an exhausted current account,
            // which this method is never called for.
            crate::usage::BalanceDecision::Failover { .. }
            | crate::usage::BalanceDecision::AllExhausted => {}
        }
    }

    /// Point the active-account override at `label` and invalidate the stale
    /// credential so the next request picks it up. Same mechanism the manual
    /// switch and exhaustion failover use - never rotates or resets a token.
    fn apply_account_override(&self, provider: ActiveProvider, label: &str) {
        match provider {
            ActiveProvider::Claude => {
                crate::auth::claude::set_active_account_override(Some(label.to_string()));
                clear_all_provider_unavailability_for_account();
                clear_all_model_unavailability_for_account();
                if let Some(anthropic) = self.anthropic_provider() {
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(anthropic.invalidate_credentials())
                    });
                }
            }
            ActiveProvider::OpenAI => {
                crate::auth::codex::set_active_account_override(Some(label.to_string()));
                clear_all_provider_unavailability_for_account();
                clear_all_model_unavailability_for_account();
                if let Some(openai) = self.openai_provider() {
                    tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(openai.invalidate_credentials())
                    });
                }
            }
            _ => {}
        }
    }

    /// Prime a peer account's unopened 5-hour window: temporarily activate it,
    /// send ONE minimal inference request to start its 5h clock, then restore
    /// the original active account. Best-effort - any failure is logged and the
    /// original account is always restored. Sends only a minimal request and
    /// never touches credentials beyond the override/invalidate path.
    fn prime_account(
        &self,
        provider: ActiveProvider,
        probe: &crate::usage::AccountUsageProbe,
        target: &str,
    ) {
        if provider != ActiveProvider::Claude {
            return;
        }
        let original = probe.current_label.clone();
        crate::logging::info(&format!(
            "{} priming rotating account '{}' 5-hour window (minimal request)",
            probe.provider.display_name(),
            target
        ));
        self.apply_account_override(provider, target);
        let result = self.anthropic_provider().map(|anthropic| {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    anthropic
                        .complete_simple("hi", "Reply with a single character.")
                        .await
                })
            })
        });
        // Always restore the original active account, whatever happened.
        self.apply_account_override(provider, &original);
        match result {
            Some(Ok(_)) => {
                let notice = format!(
                    "⚡ Primed {} account **{}**: opened its 5-hour window so it is ready when needed",
                    probe.provider.display_name(),
                    target
                );
                self.startup_notices
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(notice);
            }
            Some(Err(err)) => {
                crate::logging::info(&format!(
                    "{} priming of '{}' did not complete: {}",
                    probe.provider.display_name(),
                    target,
                    err
                ));
            }
            None => {}
        }
    }
}
