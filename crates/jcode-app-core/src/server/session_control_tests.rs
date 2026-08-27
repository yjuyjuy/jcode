#![cfg_attr(test, allow(clippy::await_holding_lock))]

use super::*;
use crate::message::{StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use crate::server::ClientConnectionInfo;
use crate::tool::Registry;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio::time::{Duration, timeout};

/// Provider that records its per-instance account pin and model in memory, so a
/// test can assert what the control surface applied without touching real auth.
#[derive(Default)]
struct AccountAwareProvider {
    model: StdMutex<String>,
    account: StdMutex<Option<String>>,
    effort: StdMutex<Option<String>>,
    set_model_calls: AtomicUsize,
}

impl AccountAwareProvider {
    fn new(model: &str) -> Self {
        Self {
            model: StdMutex::new(model.to_string()),
            account: StdMutex::new(None),
            effort: StdMutex::new(None),
            set_model_calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Provider for AccountAwareProvider {
    async fn complete(
        &self,
        _messages: &[crate::message::Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> anyhow::Result<EventStream> {
        // No test here drives a real turn.
        let stream = async_stream::stream! {
            yield Ok(StreamEvent::TextDelta(String::new()));
        };
        Ok(Box::pin(stream))
    }

    fn name(&self) -> &str {
        "account-aware"
    }

    fn model(&self) -> String {
        self.model.lock().expect("model lock").clone()
    }

    fn set_model(&self, model: &str) -> anyhow::Result<()> {
        self.set_model_calls.fetch_add(1, Ordering::SeqCst);
        *self.model.lock().expect("model lock") = model.to_string();
        Ok(())
    }

    fn available_models_for_switching(&self) -> Vec<String> {
        vec!["model-a".to_string(), "model-b".to_string()]
    }

    fn account_label(&self) -> Option<String> {
        self.account.lock().expect("account lock").clone()
    }

    fn set_account_label(&self, label: Option<String>) -> anyhow::Result<()> {
        // A test can request a failing account to exercise the error path.
        if label.as_deref() == Some("nonexistent") {
            anyhow::bail!("no account 'nonexistent'");
        }
        *self.account.lock().expect("account lock") = label;
        Ok(())
    }

    fn reasoning_effort(&self) -> Option<String> {
        self.effort.lock().expect("effort lock").clone()
    }

    fn set_reasoning_effort(&self, effort: &str) -> anyhow::Result<()> {
        *self.effort.lock().expect("effort lock") = Some(effort.to_string());
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self {
            model: StdMutex::new(self.model()),
            account: StdMutex::new(self.account_label()),
            effort: StdMutex::new(self.reasoning_effort()),
            set_model_calls: AtomicUsize::new(0),
        })
    }
}

async fn make_agent(
    session_id: &str,
    model: &str,
) -> (Arc<AccountAwareProvider>, Arc<Mutex<Agent>>) {
    let provider = Arc::new(AccountAwareProvider::new(model));
    let provider_dyn: Arc<dyn Provider> = provider.clone();
    let registry = Registry::new(Arc::clone(&provider_dyn)).await;
    let mut session = crate::session::Session::create_with_id(session_id.to_string(), None, None);
    session.model = Some(model.to_string());
    let agent = Arc::new(Mutex::new(Agent::new_with_session(
        Arc::clone(&provider_dyn),
        registry,
        session,
        None,
    )));
    (provider, agent)
}

fn connection_for(session_id: &str) -> ClientConnectionInfo {
    let (disconnect_tx, _rx) = mpsc::unbounded_channel();
    ClientConnectionInfo {
        client_id: format!("client-{session_id}"),
        session_id: session_id.to_string(),
        client_instance_id: None,
        debug_client_id: None,
        connected_at: std::time::Instant::now(),
        last_seen: std::time::Instant::now(),
        is_processing: false,
        current_tool_name: None,
        terminal_env: Vec::new(),
        disconnect_tx,
    }
}

#[allow(clippy::type_complexity)]
async fn fixture(
    entries: &[(&str, &str)],
) -> (
    SessionAgents,
    Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    Arc<RwLock<HashMap<String, SwarmMember>>>,
    HashMap<String, Arc<AccountAwareProvider>>,
) {
    let sessions: SessionAgents = Arc::new(RwLock::new(HashMap::new()));
    let connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::new()));
    let mut providers = HashMap::new();
    for (session_id, model) in entries {
        let (provider, agent) = make_agent(session_id, model).await;
        sessions
            .write()
            .await
            .insert((*session_id).to_string(), agent);
        connections
            .write()
            .await
            .insert(format!("client-{session_id}"), connection_for(session_id));
        providers.insert((*session_id).to_string(), provider);
    }
    (sessions, connections, swarm_members, providers)
}

#[tokio::test]
async fn list_sessions_reports_provider_account_model() {
    let _guard = crate::storage::lock_test_env();
    // A temp JCODE_HOME lets the health view's transcript-size proxy find a real
    // session record to stat, without touching the developer's ~/.jcode.
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let (sessions, _connections, swarm_members, providers) =
        fixture(&[("session_list_a", "model-a")]).await;
    providers["session_list_a"]
        .set_account_label(Some("claude-2".to_string()))
        .unwrap();
    providers["session_list_a"]
        .set_reasoning_effort("high")
        .unwrap();

    // Write the record after building the fixture so its byte length is exactly
    // what we assert, independent of any persistence during agent construction.
    let sessions_dir = temp_home.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
    let record = sessions_dir.join("session_list_a.json");
    let record_body = "{\"id\":\"session_list_a\"}";
    std::fs::write(&record, record_body).expect("write session record");

    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_list_sessions(1, &sessions, &swarm_members, &tx).await;

    let event = rx.try_recv().expect("session list event");
    let ServerEvent::SessionList { id, sessions: list } = event else {
        panic!("expected SessionList, got {event:?}");
    };
    assert_eq!(id, 1);
    assert_eq!(list.len(), 1);
    let info = &list[0];
    assert_eq!(info.session_id, "session_list_a");
    assert_eq!(info.provider.as_deref(), Some("account-aware"));
    assert_eq!(info.account.as_deref(), Some("claude-2"));
    // Reasoning effort rides the same live-agent snapshot as account/model.
    assert_eq!(info.effort.as_deref(), Some("high"));
    // Context-size proxy: the exact byte length of the stored record.
    assert_eq!(info.transcript_bytes, Some(record_body.len() as u64));
    // The agent may restore a provider-key-prefixed model spec on construction
    // (e.g. "claude:model-a"); the control surface reports whatever the provider
    // returns, so assert on the model suffix rather than the exact prefix.
    assert!(
        info.model.as_deref().unwrap().ends_with("model-a"),
        "unexpected model: {:?}",
        info.model
    );
    assert!(!info.is_processing);

    crate::env::remove_var("JCODE_HOME");
}

#[tokio::test]
async fn list_sessions_reports_effort_none_when_unset_and_a_context_size() {
    let _guard = crate::storage::lock_test_env();
    // A live session persists its record on construction, so the health view's
    // transcript-size proxy is present; a provider with no configured effort
    // reports None (which the wire then omits, covered by the protocol test).
    let temp_home = tempfile::TempDir::new().expect("temp home");
    crate::env::set_var("JCODE_HOME", temp_home.path());

    let (sessions, _connections, swarm_members, _providers) =
        fixture(&[("session_bare_a", "model-a")]).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_list_sessions(2, &sessions, &swarm_members, &tx).await;

    let ServerEvent::SessionList { sessions: list, .. } =
        rx.try_recv().expect("session list event")
    else {
        panic!("expected SessionList");
    };
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].effort, None);
    // A real live session has a persisted record, so the size proxy is present
    // and positive. The None (omit-on-wire) branch is asserted in the protocol
    // roundtrip test.
    assert!(
        list[0].transcript_bytes.map(|b| b > 0).unwrap_or(false),
        "live session should report a positive context size, got {:?}",
        list[0].transcript_bytes
    );

    crate::env::remove_var("JCODE_HOME");
}

#[tokio::test]
async fn switch_account_applies_to_idle_session() {
    let _guard = crate::storage::lock_test_env();
    let (sessions, _connections, swarm_members, providers) =
        fixture(&[("session_switch_a", "model-a")]).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_switch_session_account(
        2,
        Some("session_switch_a".to_string()),
        "claude-3".to_string(),
        &sessions,
        &swarm_members,
        &tx,
    )
    .await;

    let ServerEvent::SessionSwitchResult { id, results } = rx.try_recv().expect("switch result")
    else {
        panic!("expected SessionSwitchResult");
    };
    assert_eq!(id, 2);
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    assert!(!results[0].deferred);
    assert_eq!(results[0].account.as_deref(), Some("claude-3"));
    assert_eq!(
        providers["session_switch_a"].account_label().as_deref(),
        Some("claude-3")
    );
}

#[tokio::test]
async fn switch_all_sessions_reports_each() {
    let _guard = crate::storage::lock_test_env();
    let (sessions, _connections, swarm_members, providers) =
        fixture(&[("session_all_a", "model-a"), ("session_all_b", "model-a")]).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_switch_session_account(
        3,
        None,
        "openai-2".to_string(),
        &sessions,
        &swarm_members,
        &tx,
    )
    .await;

    let ServerEvent::SessionSwitchResult { results, .. } = rx.try_recv().expect("switch result")
    else {
        panic!("expected SessionSwitchResult");
    };
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.ok && !r.deferred));
    assert_eq!(
        providers["session_all_a"].account_label().as_deref(),
        Some("openai-2")
    );
    assert_eq!(
        providers["session_all_b"].account_label().as_deref(),
        Some("openai-2")
    );
}

#[tokio::test]
async fn switch_account_model_crosses_together() {
    let _guard = crate::storage::lock_test_env();
    let (sessions, _connections, swarm_members, providers) =
        fixture(&[("session_cross_a", "model-a")]).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_switch_session_account_model(
        4,
        Some("session_cross_a".to_string()),
        "openai-2".to_string(),
        "model-b".to_string(),
        &sessions,
        &swarm_members,
        &tx,
    )
    .await;

    let ServerEvent::SessionSwitchResult { results, .. } = rx.try_recv().expect("switch result")
    else {
        panic!("expected SessionSwitchResult");
    };
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    assert_eq!(results[0].model.as_deref(), Some("model-b"));
    let provider = &providers["session_cross_a"];
    assert_eq!(provider.model(), "model-b");
    assert_eq!(provider.account_label().as_deref(), Some("openai-2"));
}

#[tokio::test]
async fn switch_unknown_session_reports_failure() {
    let _guard = crate::storage::lock_test_env();
    let (sessions, _connections, swarm_members, _providers) =
        fixture(&[("session_known", "model-a")]).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_switch_session_account(
        5,
        Some("does-not-exist".to_string()),
        "claude-2".to_string(),
        &sessions,
        &swarm_members,
        &tx,
    )
    .await;

    let ServerEvent::SessionSwitchResult { results, .. } = rx.try_recv().expect("switch result")
    else {
        panic!("expected SessionSwitchResult");
    };
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert!(results[0].error.is_some());
}

#[tokio::test]
async fn switch_reports_per_session_failure() {
    let _guard = crate::storage::lock_test_env();
    let (sessions, _connections, swarm_members, _providers) =
        fixture(&[("session_fail_a", "model-a")]).await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_switch_session_account(
        6,
        Some("session_fail_a".to_string()),
        "nonexistent".to_string(),
        &sessions,
        &swarm_members,
        &tx,
    )
    .await;

    let ServerEvent::SessionSwitchResult { results, .. } = rx.try_recv().expect("switch result")
    else {
        panic!("expected SessionSwitchResult");
    };
    assert_eq!(results.len(), 1);
    assert!(!results[0].ok);
    assert!(results[0].error.as_deref().unwrap().contains("nonexistent"));
}

#[tokio::test]
async fn switch_defers_when_session_busy_and_applies_on_drain() {
    let _guard = crate::storage::lock_test_env();
    let (sessions, _connections, swarm_members, providers) =
        fixture(&[("session_busy_a", "model-a")]).await;

    let agent = sessions
        .read()
        .await
        .get("session_busy_a")
        .cloned()
        .expect("agent");
    // Simulate a turn in flight by holding the agent lock.
    let busy = agent.lock().await;

    let (tx, mut rx) = mpsc::unbounded_channel();
    handle_switch_session_account(
        7,
        Some("session_busy_a".to_string()),
        "claude-2".to_string(),
        &sessions,
        &swarm_members,
        &tx,
    )
    .await;

    let ServerEvent::SessionSwitchResult { results, .. } = rx.try_recv().expect("switch result")
    else {
        panic!("expected SessionSwitchResult");
    };
    assert_eq!(results.len(), 1);
    assert!(results[0].ok, "busy switch is accepted");
    assert!(results[0].deferred, "busy switch is deferred to next turn");
    // The account is not applied while the turn holds the lock.
    assert_eq!(providers["session_busy_a"].account_label(), None);

    // Drain the turn: releasing the lock lets the deferred applier run.
    drop(busy);

    // Poll until the spawned deferred applier lands the switch.
    let applied = timeout(Duration::from_secs(2), async {
        loop {
            if providers["session_busy_a"].account_label().as_deref() == Some("claude-2") {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        applied,
        "deferred switch should apply after the turn drains"
    );
}
