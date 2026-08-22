use super::*;

impl Agent {
    pub(super) fn note_compaction_applied(&mut self) {
        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;
    }

    pub fn poll_compaction_completion_event(&mut self) -> Option<CompactionEvent> {
        let provider_messages = self.session.messages_for_provider();
        let compaction = self.registry.compaction();
        let event = match compaction.try_write() {
            Ok(mut manager) => {
                let event = manager.poll_compaction_event_with(&provider_messages);
                if event.is_some() {
                    self.sync_session_compaction_state_from_manager(&manager);
                }
                event
            }
            Err(_) => return None,
        };

        if event.is_some() {
            self.note_compaction_applied();
            self.persist_session_best_effort("compaction completion");
        }

        event
    }

    pub fn request_manual_compaction(&mut self) -> (String, bool) {
        if !self.provider.supports_compaction() {
            return (
                "Manual compaction is not available for this provider.".to_string(),
                false,
            );
        }

        let provider = self.provider.fork();
        let messages = self.session.messages_for_provider();
        let compaction = self.registry.compaction();

        match compaction.try_write() {
            Ok(mut manager) => {
                let stats = manager.stats_with(&messages);
                let status_msg = format!(
                    "**Context Status:**\n\
                    • Messages: {} (active), {} (total history)\n\
                    • Token usage: ~{}k (estimate ~{}k) / {}k ({:.1}%)\n\
                    • Has summary: {}\n\
                    • Compacting: {}",
                    stats.active_messages,
                    stats.total_turns,
                    stats.effective_tokens / 1000,
                    stats.token_estimate / 1000,
                    manager.token_budget() / 1000,
                    stats.context_usage * 100.0,
                    if stats.has_summary { "yes" } else { "no" },
                    if stats.is_compacting {
                        "in progress..."
                    } else {
                        "no"
                    }
                );

                match manager.force_compact_with(&messages, provider) {
                    Ok(()) => (
                        format!(
                            "{}\n\n📦 **Compacting context** (manual) — summarizing older messages in the background to stay within the context window.\n\
                            The summary will be applied automatically when ready.",
                            status_msg
                        ),
                        true,
                    ),
                    Err(reason) => (
                        format!("{status_msg}\n\n⚠ **Cannot compact:** {reason}"),
                        false,
                    ),
                }
            }
            Err(_) => (
                "⚠ Cannot access compaction manager (lock held)".to_string(),
                false,
            ),
        }
    }

    /// Drain any pending `compact_context` tool request for this session and, if
    /// present, start manual compaction. Called by the turn loop at a safe point
    /// where it holds `&mut self`, since the tool cannot borrow the locked Agent
    /// while it runs. The background summary is applied at the turn boundary via
    /// the existing `CompactionFinished` path, exactly like the `/compact`
    /// command.
    pub(super) fn drain_pending_compaction_request(&mut self) {
        if !crate::tool::take_session_compaction_request(&self.session.id) {
            return;
        }
        let session_id = self.session.id.clone();
        let (message, success) = self.request_manual_compaction();
        if success {
            crate::logging::info(&format!(
                "Agent-requested manual compaction started for session {session_id}"
            ));
            crate::runtime_memory_log::emit_event(
                crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                    "manual_compaction_requested",
                    "agent_tool_compaction_started",
                )
                .with_session_id(session_id)
                .force_attribution(),
            );
        } else {
            crate::logging::info(&format!(
                "Agent-requested manual compaction not started for session {session_id}: {message}"
            ));
        }
    }

    fn is_context_limit_error(error: &str) -> bool {
        let lower = error.to_lowercase();
        lower.contains("context length")
            || lower.contains("context window")
            || lower.contains("maximum context")
            || lower.contains("max context")
            || lower.contains("token limit")
            || lower.contains("too many tokens")
            || lower.contains("prompt is too long")
            || lower.contains("input is too long")
            || lower.contains("request too large")
            || lower.contains("length limit")
            || lower.contains("maximum tokens")
            || (lower.contains("exceeded") && lower.contains("tokens"))
    }

    /// Best-effort emergency recovery after a context-limit error.
    ///
    /// Performs a synchronous hard compaction and resets provider session state,
    /// allowing the caller to retry the same turn immediately.
    pub(super) fn try_auto_compact_after_context_limit(&mut self, error: &str) -> bool {
        if crate::provider::openai_request::is_openai_encrypted_content_too_large_error(error)
            && self.try_recover_oversized_openai_native_compaction()
        {
            return true;
        }
        // A provider HTTP 413 ("request too large") is a *byte-size* failure
        // driven by inline base64 images, not a token-context overflow. Token
        // accounting deliberately undercounts images, so ordinary compaction
        // would not shrink the payload and the retry would 413 again. Strip
        // oversized images first.
        if self.try_recover_after_payload_too_large(error) {
            return true;
        }
        if !Self::is_context_limit_error(error) {
            return false;
        }
        if !self.provider.supports_compaction() {
            return false;
        }

        let context_limit = self.provider.context_window() as u64;
        let compaction = self.registry.compaction();

        let (dropped, usage_pct) = match compaction.try_write() {
            Ok(mut manager) => {
                let (dropped, usage_pct) = {
                    let all_messages = self.session.provider_messages();
                    manager.update_observed_input_tokens(context_limit);
                    let usage_pct = manager.context_usage_with(all_messages) * 100.0;
                    let dropped = match manager.hard_compact_with(all_messages) {
                        Ok(dropped) => dropped,
                        Err(reason) => {
                            logging::warn(&format!(
                                "Context-limit auto-recovery failed: hard compact failed ({})",
                                reason
                            ));
                            return false;
                        }
                    };
                    (dropped, usage_pct)
                };
                self.sync_session_compaction_state_from_manager(&manager);
                (dropped, usage_pct)
            }
            Err(_) => {
                logging::warn("Context-limit auto-recovery skipped: compaction manager lock busy");
                return false;
            }
        };

        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;

        logging::warn(&format!(
            "Context limit exceeded; auto-compacted and retrying (dropped {} messages, usage was {:.1}%)",
            dropped, usage_pct
        ));
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "auto_compaction_applied",
                "context_limit_auto_compaction",
            )
            .with_session_id(self.session.id.clone())
            .with_detail(format!(
                "dropped_messages={dropped},usage_pct={usage_pct:.1}"
            ))
            .force_attribution(),
        );

        true
    }

    /// Best-effort recovery after a provider HTTP 413 "request too large" error.
    ///
    /// This failure is caused by the serialized request body (dominated by inline
    /// base64 images) exceeding the provider's size cap, which is independent of
    /// the token context window. We strip oversized images from the persisted
    /// transcript, oldest-first, down to a conservative byte budget and reset the
    /// provider session/cache so the caller can retry the same turn immediately.
    fn try_recover_after_payload_too_large(&mut self, error: &str) -> bool {
        if !crate::compaction::is_request_payload_too_large_error(error) {
            return false;
        }

        let stripped = self
            .session
            .strip_oversized_images(crate::compaction::PAYLOAD_IMAGE_CHAR_BUDGET);
        if stripped == 0 {
            logging::warn(
                "Request-too-large recovery skipped: no oversized inline images to strip",
            );
            return false;
        }

        // The transcript changed; reseed compaction bookkeeping and reset
        // provider session/cache state so the retry sends the reduced payload.
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            let provider_messages = self.session.messages_for_provider();
            manager.reset();
            manager.set_budget(self.provider.context_window());
            if let Some(state) = self.session.compaction.as_ref() {
                manager.restore_persisted_state_with(state, &provider_messages);
            } else {
                manager.seed_restored_messages_with(&provider_messages);
            }
            self.sync_session_compaction_state_from_manager(&manager);
        }

        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;

        logging::warn(&format!(
            "Request body exceeded provider size limit; stripped {} oversized inline image(s) and retrying",
            stripped
        ));
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "payload_too_large_recovered",
                "request_payload_too_large",
            )
            .with_session_id(self.session.id.clone())
            .with_detail(format!("images_stripped={stripped}"))
            .force_attribution(),
        );

        true
    }

    fn try_recover_oversized_openai_native_compaction(&mut self) -> bool {
        let compaction = self.registry.compaction();
        let recovered = match compaction.try_write() {
            Ok(mut manager) => {
                if !manager.discard_oversized_openai_native_compaction() {
                    return false;
                }
                self.sync_session_compaction_state_from_manager(&manager);
                true
            }
            Err(_) => {
                logging::warn(
                    "OpenAI native compaction recovery skipped: compaction manager lock busy",
                );
                false
            }
        };

        if !recovered {
            return false;
        }

        self.cache_tracker.reset();
        self.locked_tools = None;
        self.provider_session_id = None;
        self.session.provider_session_id = None;

        logging::warn(
            "OpenAI native compaction payload exceeded provider size limit; discarded native state and retrying with text fallback",
        );
        crate::runtime_memory_log::emit_event(
            crate::runtime_memory_log::RuntimeMemoryLogEvent::new(
                "native_compaction_payload_recovered",
                "openai_encrypted_content_too_large",
            )
            .with_session_id(self.session.id.clone())
            .force_attribution(),
        );

        true
    }

    fn effective_context_tokens_from_usage(
        &self,
        input_tokens: u64,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
    ) -> u64 {
        // Shared heuristic (jcode-compaction-core): keeps the compaction
        // manager's observed-token feed consistent with the client-side
        // context display.
        crate::compaction::effective_context_tokens_from_usage(
            self.provider.name(),
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        )
    }

    pub(super) fn update_compaction_usage_from_stream(
        &mut self,
        input_tokens: u64,
        cache_read_input_tokens: Option<u64>,
        cache_creation_input_tokens: Option<u64>,
    ) {
        if !self.provider.uses_jcode_compaction() || input_tokens == 0 {
            return;
        }
        let observed = self.effective_context_tokens_from_usage(
            input_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
        );
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.update_observed_input_tokens(observed);
            manager.push_token_snapshot(observed);
        };
    }

    /// Push an embedding snapshot for the semantic compaction mode.
    /// Called after each assistant turn with a short text snippet.
    /// No-op if the embedding model is unavailable or mode is not semantic.
    pub(super) fn push_embedding_snapshot_if_semantic(&mut self, text: &str) {
        use crate::config::CompactionMode;
        let is_semantic = {
            let compaction = self.registry.compaction();
            compaction
                .try_read()
                .map(|m| m.mode() == CompactionMode::Semantic)
                .unwrap_or(false)
        };
        if !is_semantic {
            return;
        }
        let compaction = self.registry.compaction();
        if let Ok(mut manager) = compaction.try_write() {
            manager.push_embedding_snapshot(text);
        };
    }
}

/// How the pre-compact action sub-turn executes, matching the turn loop that
/// invoked the flow.
pub(super) enum PreCompactTurnMode {
    /// Plain (non-streaming) turn loop: the sub-turn runs silently.
    Plain,
    /// Streaming turn loop: the sub-turn streams events to the attached client.
    Streaming(mpsc::UnboundedSender<ServerEvent>),
}

/// Resolved form of a configured pre-compact action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PreCompactActionSpec {
    /// Run the named skill as an in-session sub-turn (user message `/<name>`).
    Skill(String),
    /// Inject the raw text as a user message and process it as a turn.
    Prompt(String),
    /// Run an external command via the shell before compaction.
    Command(String),
}

impl Agent {
    /// Run the configured pre-compact action ahead of a proactive/soft-threshold
    /// compaction, and in blocking mode wait for that compaction to complete and
    /// apply before returning.
    ///
    /// Called at the top of each turn-loop iteration, right before the provider
    /// request is built. It is a safe no-op unless a pre-compact action is
    /// configured, a compaction is actually due (`should_compact_with`), the
    /// context is below the critical threshold, and the flow has not already run
    /// this turn.
    ///
    /// When the pre-compact action is set, jcode runs it synchronously first (an
    /// in-session skill or prompt sub-turn, or an external command), then lets
    /// the compaction proceed. With blocking mode on, the compaction is started
    /// and waited on, so the next model call sees the compacted context; the wait
    /// is bounded and degrades to the regular in-flight apply path on timeout.
    ///
    /// The emergency hard-compact path (context-limit recovery) is deliberately
    /// out of scope: at or above the critical threshold this flow does nothing,
    /// so a context-limit emergency never blocks on a skill turn that itself
    /// needs context.
    pub(super) async fn run_pre_compact_flow_if_due(&mut self, mode: PreCompactTurnMode) {
        if self.pre_compact_flow_ran {
            return;
        }
        let (action, blocking) = {
            let compaction = self.registry.compaction();
            let Ok(manager) = compaction.try_read() else {
                return;
            };
            let Some(action) = manager.pre_compact_action() else {
                return;
            };
            let all_messages = self.session.provider_messages();
            if manager.is_at_critical_threshold_with(all_messages) {
                return;
            }
            if !manager.should_compact_with(all_messages) {
                return;
            }
            (action.to_string(), manager.blocking_compact())
        };

        self.pre_compact_flow_ran = true;
        // Hold the manager's soft-tier checks (including the sub-turn's own loop)
        // suspended until the action has run to completion, so the ordering is
        // always action first, then compaction. Hard-compact stays untouched.
        if let Ok(mut manager) = self.registry.compaction().try_write() {
            manager.set_pre_compact_in_progress(true);
        }

        let skills = self.current_skills_snapshot();
        match Self::resolve_pre_compact_action(&action, &skills) {
            Some(spec) => {
                if let Err(error) = self.run_pre_compact_action_turn(spec, mode).await {
                    logging::warn(&format!(
                        "Pre-compact action failed; continuing with compaction: {}",
                        error
                    ));
                }
            }
            None => {
                logging::warn(&format!(
                    "Pre-compact action could not be resolved ({action:?}); continuing with compaction"
                ));
            }
        }

        if let Ok(mut manager) = self.registry.compaction().try_write() {
            manager.set_pre_compact_in_progress(false);
        }

        if blocking {
            self.run_blocking_compaction().await;
        }
    }

    /// Resolve a configured pre-compact action into an executable form against
    /// the given skill registry.
    pub(super) fn resolve_pre_compact_action(
        raw: &str,
        skills: &SkillRegistry,
    ) -> Option<PreCompactActionSpec> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        if let Some(command) = raw.strip_prefix("cmd:") {
            let command = command.trim();
            return (!command.is_empty())
                .then(|| PreCompactActionSpec::Command(command.to_string()));
        }
        if let Some(prompt) = raw.strip_prefix("prompt:") {
            let prompt = prompt.trim();
            return (!prompt.is_empty())
                .then(|| PreCompactActionSpec::Prompt(prompt.to_string()));
        }
        let name = if let Some(name) = raw.strip_prefix("skill:") {
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            name
        } else {
            raw
        };
        if skills.get(name).is_some() {
            return Some(PreCompactActionSpec::Skill(name.to_string()));
        }
        // A bare string that is not an installed skill name behaves as a prompt.
        Some(PreCompactActionSpec::Prompt(raw.to_string()))
    }

    /// Execute a resolved pre-compact action.
    pub(super) async fn run_pre_compact_action_turn(
        &mut self,
        spec: PreCompactActionSpec,
        mode: PreCompactTurnMode,
    ) -> Result<()> {
        match spec {
            PreCompactActionSpec::Skill(name) => {
                let previous_skill = self.active_skill.take();
                self.active_skill = Some(name.clone());
                let result = self
                    .run_pre_compact_user_turn(&format!("/{name}"), mode)
                    .await;
                self.active_skill = previous_skill;
                result
            }
            PreCompactActionSpec::Prompt(text) => self.run_pre_compact_user_turn(&text, mode).await,
            PreCompactActionSpec::Command(command) => self.run_pre_compact_command(&command).await,
        }
    }

    /// Append the action message as a user message and run a full turn to
    /// completion (the in-session form of the pre-compact action).
    async fn run_pre_compact_user_turn(
        &mut self,
        user_message: &str,
        mode: PreCompactTurnMode,
    ) -> Result<()> {
        self.add_message(
            Role::User,
            vec![ContentBlock::Text {
                text: user_message.to_string(),
                cache_control: None,
            }],
        );
        self.session.save()?;
        // The sub-turn re-enters the turn loop, which again consults this flow
        // (guarded by the in-progress flag). Box the recursive edge so the
        // future stays a fixed size.
        match mode {
            PreCompactTurnMode::Plain => {
                Box::pin(self.run_turn(false)).await?;
            }
            PreCompactTurnMode::Streaming(event_tx) => {
                Box::pin(self.run_turn_streaming_mpsc(event_tx)).await?;
            }
        }
        Ok(())
    }

    /// Run the external-command form of the pre-compact action via the shell.
    /// Mirrors the lifecycle-hook environment conventions (`JCODE_HOOKS_DISABLED`,
    /// `JCODE_HOOK_EVENT`) so hook commands can detect a nested jcode and skip it.
    async fn run_pre_compact_command(&self, command: &str) -> Result<()> {
        let status = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env("JCODE_HOOKS_DISABLED", "1")
            .env("JCODE_HOOK_EVENT", "pre_compact")
            .env("JCODE_HOOK_SESSION_ID", self.session.id.clone())
            .status()
            .await?;
        if !status.success() {
            return Err(anyhow::anyhow!(
                "pre-compact command exited with {status}"
            ));
        }
        Ok(())
    }

    /// Start the soft-threshold compaction (blocking mode) and wait for it to
    /// complete and apply before returning, so the next model call sees the
    /// compacted context. Bounded: on timeout the turn continues and the
    /// in-flight summary is applied by the regular completion path.
    async fn run_blocking_compaction(&mut self) {
        const BLOCKING_COMPACT_WAIT: Duration = Duration::from_secs(180);
        const BLOCKING_COMPACT_POLL: Duration = Duration::from_millis(100);

        let messages = self.session.messages_for_provider();
        let compaction = self.registry.compaction();
        let mut manager = match compaction.try_write() {
            Ok(manager) => manager,
            Err(_) => {
                logging::warn("Blocking compaction skipped: compaction manager lock busy");
                return;
            }
        };
        // Starts the same background task the non-blocking path would start; the
        // only difference is that we wait for it here.
        manager.maybe_start_compaction_with(&messages, self.provider.clone());
        if !manager.is_compacting() {
            // Nothing to compact after all (cutoff/safety checks rejected it).
            return;
        }
        let deadline = Instant::now() + BLOCKING_COMPACT_WAIT;
        loop {
            manager.check_and_apply_compaction_with(&messages);
            if manager.has_compaction_event() {
                logging::info("[compaction] Blocking compaction applied before continuing the turn");
                return;
            }
            if !manager.is_compacting() {
                logging::warn(
                    "[compaction] Blocking compaction did not apply (generation failed or aborted); continuing",
                );
                return;
            }
            if Instant::now() >= deadline {
                logging::warn(
                    "[compaction] Blocking compaction timed out; continuing with the in-flight summary",
                );
                return;
            }
            drop(manager);
            tokio::time::sleep(BLOCKING_COMPACT_POLL).await;
            manager = match compaction.try_write() {
                Ok(manager) => manager,
                Err(_) => return,
            };
        }
    }
}
