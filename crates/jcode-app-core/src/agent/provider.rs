use super::*;

/// The resolved, persisted `{model, provider, effort}` after a combined
/// model+effort set. This is the *applied* state (what the session will use for
/// its next turn), which is what a caller must verify against - not the raw
/// request, since effort legitimately clamps to what the model supports (e.g.
/// `max` -> `xhigh`) and a route prefix is consumed into the provider. Serialized
/// straight onto the debug `set_model` response so a non-interactive caller can
/// read the outcome back without a second round-trip.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppliedModelEffort {
    pub model: String,
    pub provider: String,
    pub effort: Option<String>,
}

impl Agent {
    pub fn set_premium_mode(&self, mode: crate::provider::copilot::PremiumMode) {
        self.provider.set_premium_mode(mode);
    }

    pub fn premium_mode(&self) -> crate::provider::copilot::PremiumMode {
        self.provider.premium_mode()
    }

    pub fn provider_fork(&self) -> Arc<dyn Provider> {
        self.provider.fork()
    }

    pub fn provider_handle(&self) -> Arc<dyn Provider> {
        Arc::clone(&self.provider)
    }

    pub fn available_models(&self) -> Vec<&'static str> {
        self.provider.available_models()
    }

    pub fn available_models_for_switching(&self) -> Vec<String> {
        self.provider.available_models_for_switching()
    }

    pub fn available_models_display(&self) -> Vec<String> {
        self.provider.available_models_display()
    }

    pub fn model_routes(&self) -> Vec<crate::provider::ModelRoute> {
        self.provider.model_routes()
    }

    pub fn model_catalog_snapshot(&self) -> jcode_provider_core::ModelCatalogSnapshot {
        jcode_provider_core::ModelCatalogSnapshot::new(
            Some(self.provider_name()),
            Some(self.provider_model()),
            self.available_models_display(),
            self.model_routes(),
        )
    }

    pub fn registry(&self) -> Registry {
        self.registry.clone()
    }

    pub async fn compaction_mode(&self) -> crate::config::CompactionMode {
        self.registry.compaction().read().await.mode()
    }

    pub async fn set_compaction_mode(&self, mode: crate::config::CompactionMode) -> Result<()> {
        let compaction = self.registry.compaction();
        let mut manager = compaction.write().await;
        manager.set_mode(mode);
        Ok(())
    }

    pub fn provider_messages(&mut self) -> Vec<Message> {
        self.session.messages_for_provider()
    }

    pub fn set_model(&mut self, model: &str) -> Result<()> {
        self.set_model_from_provider_state_event(
            model,
            crate::provider::ProviderModelSelectionSource::User,
        )
    }

    pub fn set_route_selection(
        &mut self,
        selection: &crate::provider::RouteSelection,
    ) -> Result<()> {
        self.set_route_selection_from_provider_state_event(
            selection,
            crate::provider::ProviderModelSelectionSource::User,
        )
    }

    /// The account label the active provider runtime is currently pinned to, if
    /// any. `None` means the session follows the process-global active account.
    pub fn account_label(&self) -> Option<String> {
        self.provider.account_label()
    }

    /// Pin this session's active provider to a specific account. The pin takes
    /// effect on the next turn: an in-flight request keeps the account it
    /// started with, so a switch never interrupts a turn (drain semantics).
    ///
    /// Resets the provider session so the next turn re-establishes context under
    /// the new account.
    pub fn set_account_label(&mut self, label: Option<String>) -> Result<()> {
        self.provider.set_account_label(label)?;
        self.reset_provider_session();
        self.log_env_snapshot("set_account_label");
        Ok(())
    }

    /// Atomically switch this session's account and model together. This is the
    /// provider-crossing case where the new provider does not serve the current
    /// model, so account and model must move as one step. The model spec is
    /// applied first (which activates the target provider runtime and may cross
    /// providers), then the account is pinned on that now-active runtime.
    pub fn switch_account_and_model(&mut self, label: Option<String>, model: &str) -> Result<()> {
        self.set_model_from_provider_state_event(
            model,
            crate::provider::ProviderModelSelectionSource::User,
        )?;
        self.provider.set_account_label(label)?;
        self.reset_provider_session();
        self.log_env_snapshot("switch_account_and_model");
        Ok(())
    }

    pub(crate) fn set_route_selection_from_auth(
        &mut self,
        selection: &crate::provider::RouteSelection,
    ) -> Result<()> {
        self.set_route_selection_from_provider_state_event(
            selection,
            crate::provider::ProviderModelSelectionSource::Auth,
        )
    }

    fn set_route_selection_from_provider_state_event(
        &mut self,
        selection: &crate::provider::RouteSelection,
        source: crate::provider::ProviderModelSelectionSource,
    ) -> Result<()> {
        self.provider.set_route_selection(selection)?;
        let resolved_model = self.provider.model();
        self.session.provider_key = Some(selection.runtime_key.stable_id());
        self.session.route_api_method = Some(selection.api_method.clone());
        self.session.model = Some(self.provider_model());
        let event = crate::provider::ProviderStateEvent::selected_model(source, resolved_model);
        self.provider_runtime_state.apply(event);
        self.persist_session_best_effort("route selection");
        self.log_env_snapshot("set_route_selection");
        Ok(())
    }

    pub(crate) fn set_model_from_auth(&mut self, model: &str) -> Result<()> {
        self.set_model_from_provider_state_event(
            model,
            crate::provider::ProviderModelSelectionSource::Auth,
        )
    }

    fn set_model_from_provider_state_event(
        &mut self,
        model: &str,
        source: crate::provider::ProviderModelSelectionSource,
    ) -> Result<()> {
        crate::provider::set_model_with_auth_refresh(self.provider.as_ref(), model)?;
        let resolved_model = self.provider.model();
        self.session.provider_key =
            crate::provider::MultiProvider::session_provider_key_after_model_switch(
                model,
                self.provider.name(),
                self.session.provider_key.as_deref(),
            );
        self.session.model = Some(self.provider_model());
        let event = crate::provider::ProviderStateEvent::selected_model(source, resolved_model);
        self.provider_runtime_state.apply(event);
        self.persist_session_best_effort("model selection");
        self.log_env_snapshot("set_model");
        Ok(())
    }

    pub(crate) fn provider_model_selection_generation(&self) -> u64 {
        self.provider_runtime_state.selection_generation()
    }

    pub(crate) fn user_selected_provider_model_after(&self, generation: u64) -> bool {
        self.provider_runtime_state.user_selected_after(generation)
    }

    pub fn restore_reasoning_effort_from_session(&mut self) {
        if let Some(effort) = self.session.reasoning_effort.clone() {
            if let Err(e) = self.provider.set_reasoning_effort(&effort) {
                crate::logging::error(&format!(
                    "Failed to restore session reasoning effort '{}': {}",
                    effort, e
                ));
            }
        } else {
            self.session.reasoning_effort = self.provider.reasoning_effort();
        }
        // Mirror the effort into the deadlock-free side-table so server handlers
        // (e.g. the swarm seed handler) can learn this session's effort without
        // taking the agent lock.
        crate::session_effort::record_session_effort(
            &self.session.id,
            self.session.reasoning_effort.as_deref(),
        );
    }

    pub fn set_reasoning_effort(&mut self, effort: &str) -> Result<Option<String>> {
        self.provider.set_reasoning_effort(effort)?;
        let current = self.provider.reasoning_effort();
        self.session.reasoning_effort = current.clone();
        // Keep the side-table in sync (see `restore_reasoning_effort_from_session`).
        crate::session_effort::record_session_effort(&self.session.id, current.as_deref());
        self.log_env_snapshot("set_reasoning_effort");
        self.session.save()?;
        Ok(current)
    }

    /// The reasoning effort the active provider will use for the next request,
    /// or `None` when no effort is configured (or the provider has no notion of
    /// effort). This mirrors `session.reasoning_effort` after any successful
    /// `set_reasoning_effort`, so it is readback-verifiable.
    pub fn reasoning_effort(&self) -> Option<String> {
        self.provider.reasoning_effort()
    }

    /// Atomically set this session's model and (optionally) its reasoning
    /// effort, persisting both to the session store, and return the resulting
    /// applied state `{model, provider, effort}` for readback verification.
    ///
    /// The model is applied first because a provider's set of valid efforts
    /// depends on the target model (e.g. Anthropic re-clamps a stored effort for
    /// the new model, and `xhigh`/`max` are model-gated). If the effort is then
    /// rejected, the model change is rolled back so the operation is all-or-
    /// nothing: a caller never lands in the partial-apply state (new model, old
    /// or wrong effort) that silently drifts a fleet worker. A bad model or a
    /// bad effort surfaces as an `Err` here rather than a silent no-op.
    pub fn set_model_and_effort(
        &mut self,
        model: &str,
        effort: Option<&str>,
    ) -> Result<AppliedModelEffort> {
        let prev_model = self.provider_model();
        let prev_effort = self.reasoning_effort();

        // Applying the model may cross providers and fail loudly for an unknown
        // model; nothing is persisted in that case.
        self.set_model(model)?;

        if let Some(effort) = effort
            && let Err(err) = self.set_reasoning_effort(effort)
        {
            // Roll the model back to the pre-call state so the session is not
            // left on the new model with a stale/wrong effort. Restoration is
            // best-effort: the original model was valid a moment ago, so this
            // normally succeeds; if it cannot, log it but still surface the
            // original effort error rather than masking it.
            if let Err(rollback_err) = self.set_model(&prev_model) {
                crate::logging::error(&format!(
                    "Failed to roll back model to '{}' after effort '{}' was rejected: {}",
                    prev_model, effort, rollback_err
                ));
            }
            // Restore the prior effort (or clear to the provider default when
            // none was configured; `default` normalizes to "no effort").
            let restore = prev_effort.as_deref().unwrap_or("default");
            if let Err(restore_err) = self.set_reasoning_effort(restore) {
                crate::logging::error(&format!(
                    "Failed to restore reasoning effort '{}' after rollback: {}",
                    restore, restore_err
                ));
            }
            return Err(err);
        }

        Ok(AppliedModelEffort {
            model: self.provider_model(),
            provider: self.provider_name(),
            effort: self.reasoning_effort(),
        })
    }

    pub fn subagent_model(&self) -> Option<String> {
        self.session.subagent_model.clone()
    }

    pub fn set_subagent_model(&mut self, model: Option<String>) -> Result<()> {
        self.session.subagent_model = model;
        self.log_env_snapshot("set_subagent_model");
        self.session.save()?;
        Ok(())
    }

    pub fn session_provider_key(&self) -> Option<String> {
        self.session.provider_key.clone()
    }

    /// API method/runtime route used to select the active model (e.g.
    /// "openai-api", "claude-oauth", "openai-compatible:nvidia-nim"). Spawned
    /// swarm agents inherit this so they reconstruct the coordinator's exact
    /// auth route instead of falling back to the config default.
    pub fn session_route_api_method(&self) -> Option<String> {
        self.session.route_api_method.clone()
    }

    /// The credential the active provider will use for the next request, when
    /// the provider distinguishes OAuth (subscription) from API key (cost).
    /// Resolved authoritatively here so remote clients can render billing/usage
    /// without re-deriving it from the provider name.
    pub fn active_resolved_credential(&self) -> Option<jcode_provider_core::ResolvedCredential> {
        self.provider.active_resolved_credential()
    }

    pub fn set_session_provider_key(&mut self, provider_key: Option<String>) {
        self.session.provider_key = provider_key;
    }

    pub fn rename_session_title(&mut self, title: Option<String>) -> Result<String> {
        self.session.rename_title(title);
        self.log_env_snapshot("rename_session");
        self.session.save()?;
        Ok(self.session.display_title_or_name().to_string())
    }

    pub fn autoreview_enabled(&self) -> Option<bool> {
        self.session.autoreview_enabled
    }

    pub fn set_autoreview_enabled(&mut self, enabled: bool) -> Result<()> {
        self.session.autoreview_enabled = Some(enabled);
        self.log_env_snapshot("set_autoreview_enabled");
        self.session.save()?;
        Ok(())
    }

    pub fn autojudge_enabled(&self) -> Option<bool> {
        self.session.autojudge_enabled
    }

    pub fn set_autojudge_enabled(&mut self, enabled: bool) -> Result<()> {
        self.session.autojudge_enabled = Some(enabled);
        self.log_env_snapshot("set_autojudge_enabled");
        self.session.save()?;
        Ok(())
    }

    /// Set the working directory for this session
    pub fn set_working_dir(&mut self, dir: &str) {
        if self.session.working_dir.as_deref() == Some(dir) {
            return;
        }
        self.session.working_dir = Some(dir.to_string());
        self.refresh_agents_md_snapshot();
        self.session.refresh_initial_session_context_message();
        self.log_env_snapshot("working_dir");
    }

    /// Get the working directory for this session
    pub fn working_dir(&self) -> Option<&str> {
        self.session.working_dir.as_deref()
    }

    /// Get the stored messages (for transcript export)
    pub fn messages(&self) -> &[StoredMessage] {
        &self.session.messages
    }
}
