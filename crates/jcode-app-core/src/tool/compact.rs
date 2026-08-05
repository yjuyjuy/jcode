//! `compact_context` tool - let a running agent trigger compaction of its own
//! context mid-turn.
//!
//! This is ordinary self-management, not privileged debug control, so the tool
//! is registered by default and is NOT gated behind `debug_control_allowed()`.
//! It only ever acts on the caller's own session (from `ToolContext.session_id`);
//! it deliberately offers no way to compact another session.
//!
//! ## Why the work is deferred
//!
//! The real compaction entry point is `Agent::request_manual_compaction`, which
//! needs the caller's live session messages and a forked provider - both owned
//! by the `Agent`. The `Agent` is exclusively locked (`Arc<Mutex<Agent>>`)
//! across its own turn while this tool runs, so the tool cannot borrow it. It
//! therefore records a per-session request (`request_session_compaction`) that
//! the agent turn loop drains at the next safe point and runs on `&mut Agent`,
//! exactly like the `/compact` command. The background summary is then applied
//! at the turn boundary via the existing `CompactionFinished` path.

use crate::compaction::{CompactionManager, MANUAL_COMPACT_MIN_THRESHOLD};
use crate::provider::Provider;
use crate::tool::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct CompactContextTool {
    /// Shared compaction manager for the session, used to read current context
    /// usage synchronously for an informative "before" line.
    compaction: Arc<RwLock<CompactionManager>>,
    /// The session's live provider, used to report clearly (not error) when the
    /// active provider does not support compaction.
    provider: Arc<dyn Provider>,
}

impl CompactContextTool {
    pub fn new(compaction: Arc<RwLock<CompactionManager>>, provider: Arc<dyn Provider>) -> Self {
        Self {
            compaction,
            provider,
        }
    }
}

#[async_trait]
impl Tool for CompactContextTool {
    fn name(&self) -> &str {
        "compact_context"
    }

    fn description(&self) -> &str {
        "Compact your own conversation context now, summarizing older messages to \
         free up the context window. Acts only on your current session. Useful \
         when you are deep in a long task and want to proactively reclaim space \
         instead of waiting for automatic compaction. The summary is applied at \
         the end of the current turn."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "intent": super::intent_schema_property(),
            },
            "required": []
        })
    }

    async fn execute(&self, _input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        let title = "compact_context".to_string();

        // Clear, non-error explanation when the provider cannot compact.
        if !self.provider.supports_compaction() {
            return Ok(ToolOutput::new(format!(
                "Context compaction is not available for the current provider ({}). \
                 No compaction was triggered.",
                self.provider.name()
            ))
            .with_title(title));
        }

        // Read current usage synchronously for an informative before-line. The
        // manager tracks observed input tokens even without the full message
        // list, so this is a useful estimate without touching the Agent.
        let usage_line = match self.compaction.try_read() {
            Ok(manager) => {
                let usage = manager.context_usage() * 100.0;
                let budget_k = manager.token_budget() / 1000;
                if usage < MANUAL_COMPACT_MIN_THRESHOLD * 100.0 {
                    format!(
                        "Current context usage is low (~{usage:.1}% of {budget_k}k). \
                         Compaction may report that there is little to compact."
                    )
                } else {
                    format!("Current context usage is ~{usage:.1}% of {budget_k}k.")
                }
            }
            Err(_) => "Current context usage is unavailable (compaction busy).".to_string(),
        };

        // Defer the actual compaction to the turn loop, which owns `&mut Agent`.
        super::request_session_compaction(&ctx.session_id);

        crate::logging::info(&format!(
            "[tool:compact_context] queued manual compaction for session {}",
            ctx.session_id
        ));

        Ok(ToolOutput::new(format!(
            "{usage_line}\n\nManual context compaction requested. Older messages \
             will be summarized in the background and the summary applied at the \
             end of this turn."
        ))
        .with_title(title))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Message, ToolDefinition};
    use crate::provider::EventStream;
    use crate::tool::{Registry, ToolContext, ToolExecutionMode};

    /// Minimal provider whose compaction support is configurable, so we can
    /// exercise both the supported and unsupported branches of the tool.
    struct FakeProvider {
        supports: bool,
    }

    #[async_trait]
    impl Provider for FakeProvider {
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _system: &str,
            _resume_session_id: Option<&str>,
        ) -> Result<EventStream> {
            Err(anyhow::anyhow!("not used in compact_context tests"))
        }

        fn name(&self) -> &str {
            "fake"
        }

        fn fork(&self) -> Arc<dyn Provider> {
            Arc::new(FakeProvider {
                supports: self.supports,
            })
        }

        fn supports_compaction(&self) -> bool {
            self.supports
        }
    }

    fn ctx(session_id: &str) -> ToolContext {
        ToolContext {
            session_id: session_id.to_string(),
            message_id: "msg".to_string(),
            tool_call_id: "call".to_string(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: ToolExecutionMode::AgentTurn,
        }
    }

    #[tokio::test]
    async fn queues_request_and_reports_when_supported() {
        let compaction = Arc::new(RwLock::new(CompactionManager::new()));
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider { supports: true });
        let tool = CompactContextTool::new(compaction, provider);

        let session_id = "compact-tool-supported-session";
        // Ensure a clean slate for this session key.
        let _ = crate::tool::take_session_compaction_request(session_id);

        let out = tool
            .execute(json!({}), ctx(session_id))
            .await
            .expect("tool executes");
        assert!(
            out.output.contains("Manual context compaction requested"),
            "unexpected output: {}",
            out.output
        );
        // The request must be recorded for the caller's own session and consumed
        // exactly once by the turn loop's drain.
        assert!(
            crate::tool::take_session_compaction_request(session_id),
            "compaction request should be pending for the caller's session"
        );
        assert!(
            !crate::tool::take_session_compaction_request(session_id),
            "the request should only be consumable once"
        );
    }

    #[tokio::test]
    async fn does_not_queue_when_provider_unsupported() {
        let compaction = Arc::new(RwLock::new(CompactionManager::new()));
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider { supports: false });
        let tool = CompactContextTool::new(compaction, provider);

        let session_id = "compact-tool-unsupported-session";
        let _ = crate::tool::take_session_compaction_request(session_id);

        let out = tool
            .execute(json!({}), ctx(session_id))
            .await
            .expect("tool executes");
        assert!(
            out.output.contains("not available"),
            "unexpected output: {}",
            out.output
        );
        // No request should be queued when the provider cannot compact.
        assert!(
            !crate::tool::take_session_compaction_request(session_id),
            "no compaction request should be queued for an unsupported provider"
        );
    }

    #[tokio::test]
    async fn compact_context_is_registered_by_default() {
        let provider: Arc<dyn Provider> = Arc::new(FakeProvider { supports: true });
        let registry = Registry::new(provider).await;
        let names = registry.tool_names().await;
        assert!(
            names.iter().any(|n| n == "compact_context"),
            "compact_context should be in the default tool listing, got: {names:?}"
        );
    }
}
