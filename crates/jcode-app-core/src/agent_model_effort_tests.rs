use crate::agent::Agent;
use crate::message::{Message, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::tool::Registry;
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

/// Minimal effort-aware provider used to exercise the combined model+effort
/// set on the agent layer without booting a real provider runtime. It models
/// the two behaviors that make the combined op non-trivial: `set_model`
/// rejects an unknown model (loud failure), and `set_reasoning_effort` both
/// rejects an unknown effort and clamps `max` down to `xhigh` (a legitimate
/// provider-side normalization, so callers must verify against applied state,
/// not the raw request).
#[derive(Clone)]
struct EffortAwareProvider {
    model: Arc<std::sync::Mutex<String>>,
    effort: Arc<std::sync::Mutex<Option<String>>>,
    known_models: Arc<Vec<String>>,
}

impl EffortAwareProvider {
    fn new(model: &str) -> Self {
        Self {
            model: Arc::new(std::sync::Mutex::new(model.to_string())),
            effort: Arc::new(std::sync::Mutex::new(None)),
            known_models: Arc::new(vec![
                "model-a".to_string(),
                "model-b".to_string(),
                "deepseek-v4-flash".to_string(),
            ]),
        }
    }
}

#[async_trait]
impl Provider for EffortAwareProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        unreachable!("EffortAwareProvider does not complete requests")
    }

    fn name(&self) -> &str {
        "effort-aware"
    }

    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    fn set_model(&self, request: &str) -> Result<()> {
        if !self.known_models.iter().any(|m| m == request) {
            anyhow::bail!("Model {} not supported by effort-aware provider", request);
        }
        *self.model.lock().unwrap() = request.to_string();
        Ok(())
    }

    fn reasoning_effort(&self) -> Option<String> {
        self.effort.lock().unwrap().clone()
    }

    fn set_reasoning_effort(&self, effort: &str) -> Result<()> {
        let requested = effort.trim().to_ascii_lowercase();
        let normalized = match requested.as_str() {
            "default" | "auto" | "" => None,
            "none" | "low" | "medium" | "high" | "xhigh" => Some(requested),
            // Clamp `max` down to `xhigh`, mirroring how a real provider
            // normalizes an unsupported-but-valid ceiling for the model.
            "max" => Some("xhigh".to_string()),
            other => anyhow::bail!(
                "Unsupported effort '{}'; expected none|low|medium|high|xhigh|max",
                other
            ),
        };
        *self.effort.lock().unwrap() = normalized;
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

#[tokio::test]
async fn set_model_and_effort_persists_both_and_is_readback_verifiable() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(EffortAwareProvider::new("model-a"));
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let applied = agent
        .set_model_and_effort("model-b", Some("high"))
        .expect("combined set should succeed");
    assert_eq!(applied.model, "model-b");
    assert_eq!(applied.provider, "effort-aware");
    assert_eq!(applied.effort.as_deref(), Some("high"));

    // Both getters reflect the change.
    assert_eq!(agent.provider_model(), "model-b");
    assert_eq!(agent.reasoning_effort().as_deref(), Some("high"));

    // Both survive to the session store, so a fresh readback confirms them.
    let persisted = crate::session::Session::load(agent.session_id()).expect("load saved session");
    assert_eq!(persisted.model.as_deref(), Some("model-b"));
    assert_eq!(persisted.reasoning_effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn set_model_and_effort_reports_clamped_applied_effort() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(EffortAwareProvider::new("model-a"));
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // `max` clamps to `xhigh` in this provider; the applied state must report
    // the clamped value so a caller verifies against reality, not the request.
    let applied = agent
        .set_model_and_effort("model-b", Some("max"))
        .expect("combined set should succeed");
    assert_eq!(applied.effort.as_deref(), Some("xhigh"));
    assert_eq!(agent.reasoning_effort().as_deref(), Some("xhigh"));
}

#[tokio::test]
async fn set_model_and_effort_is_idempotent() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(EffortAwareProvider::new("model-a"));
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let first = agent
        .set_model_and_effort("deepseek-v4-flash", Some("low"))
        .expect("first set should succeed");
    let second = agent
        .set_model_and_effort("deepseek-v4-flash", Some("low"))
        .expect("second set should succeed");
    assert_eq!(first.model, second.model);
    assert_eq!(first.effort, second.effort);
    assert_eq!(agent.provider_model(), "deepseek-v4-flash");
    assert_eq!(agent.reasoning_effort().as_deref(), Some("low"));
}

#[tokio::test]
async fn set_model_and_effort_bad_model_errors_loudly() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(EffortAwareProvider::new("model-a"));
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let err = agent
        .set_model_and_effort("no-such-model", Some("high"))
        .expect_err("unknown model must error, not silently no-op");
    assert!(err.to_string().contains("no-such-model"), "err: {err}");
    // The failed model set left the original model in place.
    assert_eq!(agent.provider_model(), "model-a");
    assert_eq!(agent.reasoning_effort(), None);
}

#[tokio::test]
async fn set_model_and_effort_bad_effort_rolls_back_model() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(EffortAwareProvider::new("model-a"));
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    // Seed a known-good starting effort so we can prove rollback restores it.
    agent
        .set_reasoning_effort("low")
        .expect("seed effort should succeed");

    let err = agent
        .set_model_and_effort("model-b", Some("bogus-effort"))
        .expect_err("unknown effort must error, not silently no-op");
    assert!(err.to_string().contains("bogus-effort"), "err: {err}");

    // Atomic: the model rolled back to its pre-call value and the effort is the
    // original, so the session never lands on the new model with a wrong effort.
    assert_eq!(agent.provider_model(), "model-a");
    assert_eq!(agent.reasoning_effort().as_deref(), Some("low"));
    let persisted = crate::session::Session::load(agent.session_id()).expect("load saved session");
    assert_eq!(persisted.model.as_deref(), Some("model-a"));
    assert_eq!(persisted.reasoning_effort.as_deref(), Some("low"));
}

#[tokio::test]
async fn set_model_and_effort_without_effort_leaves_effort_unchanged() {
    let _guard = crate::storage::lock_test_env();
    let provider: Arc<dyn Provider> = Arc::new(EffortAwareProvider::new("model-a"));
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    agent
        .set_reasoning_effort("medium")
        .expect("seed effort should succeed");
    let applied = agent
        .set_model_and_effort("model-b", None)
        .expect("model-only set should succeed");
    assert_eq!(applied.model, "model-b");
    assert_eq!(applied.effort.as_deref(), Some("medium"));
    assert_eq!(agent.reasoning_effort().as_deref(), Some("medium"));
}
