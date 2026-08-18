// Regression: a 429 on the active Anthropic account with a healthy sibling
// account present must retry on the sibling account and MUST NOT emit a
// cross-provider `ProviderFailoverPrompt`. A drained *account* is not a dead
// *provider*.
//
// Bug mechanism (reproduced before the fix): the mid-turn reactive 429 account
// switch records a transient, account-scoped provider-unavailability mark. When
// the very next failover pass runs while that mark still applies to the active
// provider, the pre-attempt `provider_unavailability_detail_for_account` check
// (mod.rs) short-circuited with `continue`, skipping the same-provider account
// failover entirely and jumping to the cross-provider prompt (a ~140k-token
// resend to OpenAI). The fix tries this provider's sibling accounts at that
// check before ever falling through to the cross-provider prompt.

use std::sync::atomic::{AtomicUsize, Ordering};

/// A Claude runtime stub that streams a single text delta when the active
/// account override is the healthy sibling, and returns a 429 rate-limit error
/// while the drained account is still active. It records which account labels
/// it was asked to complete on so the test can assert the sibling was tried.
struct AccountAwareClaudeStub {
    healthy_label: &'static str,
    attempts: Arc<std::sync::Mutex<Vec<String>>>,
    completions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for AccountAwareClaudeStub {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let active = crate::auth::claude::active_account_label().unwrap_or_default();
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(active.clone());

        if active == self.healthy_label {
            self.completions.fetch_add(1, Ordering::SeqCst);
            let event = Ok(crate::message::StreamEvent::TextDelta("ok".to_string()));
            let stream = futures::stream::once(async move { event });
            Ok(Box::pin(stream))
        } else {
            // The drained account keeps returning a 429 rate-limit error.
            Err(anyhow::anyhow!(
                "Anthropic request failed: 429 Too Many Requests (rate_limit_exceeded)"
            ))
        }
    }

    fn name(&self) -> &str {
        "claude"
    }

    fn model(&self) -> String {
        "claude-fable-5".to_string()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(AccountAwareClaudeStub {
            healthy_label: self.healthy_label,
            attempts: Arc::clone(&self.attempts),
            completions: Arc::clone(&self.completions),
        })
    }
}

fn save_two_claude_accounts(active_label: &str) {
    for label in ["claude-1", "claude-2"] {
        crate::auth::claude::upsert_account(crate::auth::claude::AnthropicAccount {
            label: label.to_string(),
            access: format!("access-{label}"),
            refresh: format!("refresh-{label}"),
            expires: i64::MAX,
            email: Some(format!("{label}@example.com")),
            scopes: Vec::new(),
            subscription_type: None,
        })
        .expect("save test Claude account");
    }
    crate::auth::claude::set_active_account_override(Some(active_label.to_string()));
}

#[test]
fn reactive_429_stays_on_healthy_claude_sibling_without_cross_provider_prompt() {
    with_clean_provider_test_env(|| {
        let runtime = enter_test_runtime();
        let _enter = runtime.enter();

        // Two Anthropic accounts: claude-2 drained (active), claude-1 healthy.
        save_two_claude_accounts("claude-2");
        crate::usage::seed_anthropic_account_usage_for_tests("claude-2", 1.0, 1.0);
        crate::usage::seed_anthropic_account_usage_for_tests("claude-1", 0.07, 0.10);

        let attempts = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let completions = Arc::new(AtomicUsize::new(0));
        let claude_stub: Arc<dyn Provider> = Arc::new(AccountAwareClaudeStub {
            healthy_label: "claude-1",
            attempts: Arc::clone(&attempts),
            completions: Arc::clone(&completions),
        });

        let provider = MultiProvider {
            claude: RwLock::new(None),
            anthropic: RwLock::new(Some(claude_stub)),
            openai: RwLock::new(Some(test_openai_runtime() as Arc<dyn Provider>)),
            copilot_api: RwLock::new(None),
            antigravity: RwLock::new(None),
            gemini: RwLock::new(None),
            cursor: RwLock::new(None),
            bedrock: RwLock::new(None),
            openrouter: RwLock::new(None),
            openai_compatible_profiles: RwLock::new(std::collections::HashMap::new()),
            active_openai_compatible_profile: RwLock::new(None),
            active: RwLock::new(ActiveProvider::Claude),
            use_claude_cli: false,
            startup_notices: RwLock::new(Vec::new()),
            initial_provider: Some(ActiveProvider::Claude),
            routes_memo: std::sync::Mutex::new(None),
            post_auth_refreshes_pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        };

        // Simulate the state a mid-turn reactive 429 switch leaves behind: the
        // account-scoped provider-unavailability mark recorded while the drained
        // account (claude-2) was still active.
        crate::auth::claude::set_active_account_override(Some("claude-2".to_string()));
        record_provider_unavailable_for_account("anthropic", "reactive 429 rate-limit switch");
        assert!(
            provider_unavailability_detail_for_account("claude").is_some(),
            "precondition: the drained account must carry the reactive mark"
        );

        let messages = vec![Message::user("hello")];

        let result = runtime.block_on(async {
            let mut stream = provider
                .complete(&messages, &[], "system", None)
                .await
                .expect("completion must succeed on the healthy Claude sibling");
            let mut text = String::new();
            while let Some(event) = futures::StreamExt::next(&mut stream).await {
                if let Ok(crate::message::StreamEvent::TextDelta(delta)) = event {
                    text.push_str(&delta);
                }
            }
            text
        });

        // 1. The completion succeeded on Claude (not a cross-provider error).
        assert_eq!(result, "ok");
        assert!(
            completions.load(Ordering::SeqCst) >= 1,
            "the healthy Claude sibling must have served the completion"
        );

        // 2. It stayed on Claude: the active provider is still Claude, never
        //    switched to OpenAI.
        assert_eq!(
            provider.active_provider(),
            ActiveProvider::Claude,
            "must not fail over to a different provider"
        );

        // 3. The sibling account was actually attempted.
        let attempted = attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert!(
            attempted.iter().any(|label| label == "claude-1"),
            "the healthy sibling account claude-1 must have been tried, attempts={attempted:?}"
        );

        // 4. Clean up the process-global override for the next test.
        crate::auth::claude::set_active_account_override(None);
        clear_all_provider_unavailability_for_account();
    });
}
