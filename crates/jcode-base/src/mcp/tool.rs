//! MCP Tool - wraps MCP server tools for jcode's tool system

use super::manager::McpManager;
use super::protocol::{ContentBlock, McpToolDef};
use anyhow::Result;
use async_trait::async_trait;
use jcode_tool_core::{Tool, ToolContext};
use jcode_tool_types::ToolOutput;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A tool that proxies to an MCP server
pub struct McpTool {
    server_name: String,
    tool_def: McpToolDef,
    manager: Arc<RwLock<McpManager>>,
}

impl McpTool {
    pub fn new(
        server_name: String,
        tool_def: McpToolDef,
        manager: Arc<RwLock<McpManager>>,
    ) -> Self {
        Self {
            server_name,
            tool_def,
            manager,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        // This will be overridden in registration with prefixed name
        &self.tool_def.name
    }

    fn description(&self) -> &str {
        self.tool_def.description.as_deref().unwrap_or("MCP tool")
    }

    fn parameters_schema(&self) -> Value {
        self.tool_def.input_schema.clone()
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolOutput> {
        let mut input = if input.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            input
        };
        // `intent` is a jcode-injected display-only parameter (see
        // ensure_intent_in_schema). Strip it before forwarding unless the
        // MCP server's own schema declares an `intent` property.
        let server_declares_intent = self
            .tool_def
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .is_some_and(|p| p.contains_key("intent"));
        if !server_declares_intent && let Some(object) = input.as_object_mut() {
            object.remove("intent");
        }
        let manager = self.manager.read().await;
        let result = manager
            .call_tool(&self.server_name, &self.tool_def.name, input)
            .await?;

        // Convert MCP content blocks into the output text plus any image blocks
        // (routed into ToolOutput.images so the model actually sees the pixels).
        let (output, images) = convert_content_blocks(result.content);
        let title = format!("mcp:{}:{}", self.server_name, self.tool_def.name);

        if result.is_error {
            let mut out = ToolOutput::new(format!("Error: {}", output)).with_title(title);
            out.images = images;
            Ok(out)
        } else {
            let mut out = ToolOutput::new(output).with_title(title);
            out.images = images;
            Ok(out)
        }
    }
}

pub fn dispatch_name(server_name: &str, tool_name: &str) -> String {
    format!("mcp__{}__{}", server_name, tool_name).replace('-', "_")
}

/// Convert MCP content blocks into (joined output text, image blocks).
///
/// Image blocks are routed into the returned `ToolImage` vec (which the caller
/// puts on `ToolOutput.images`, the model-visible image channel that native
/// tools populate via `with_image`) instead of being stringified away. A short
/// text breadcrumb is still emitted for every image so text-only providers, and
/// logs, retain a trace of what was attached.
fn convert_content_blocks(
    content: Vec<ContentBlock>,
) -> (String, Vec<jcode_tool_types::ToolImage>) {
    let mut output_parts = Vec::new();
    let mut images: Vec<jcode_tool_types::ToolImage> = Vec::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => {
                output_parts.push(text);
            }
            ContentBlock::Image { data, mime_type } => {
                output_parts.push(format!(
                    "[Image: {} ({} bytes) - attached to model]",
                    mime_type,
                    data.len()
                ));
                images.push(jcode_tool_types::ToolImage {
                    media_type: mime_type,
                    data,
                    label: None,
                });
            }
            ContentBlock::Resource { resource } => {
                if let Some(text) = resource.text {
                    output_parts.push(text);
                } else if let Some(blob) = resource.blob {
                    output_parts.push(format!(
                        "[Resource: {} ({} bytes)]",
                        resource.uri,
                        blob.len()
                    ));
                } else {
                    output_parts.push(format!("[Resource: {}]", resource.uri));
                }
            }
        }
    }
    (output_parts.join("\n"), images)
}

/// Create tools from an MCP manager
pub async fn create_mcp_tools(manager: Arc<RwLock<McpManager>>) -> Vec<(String, Arc<dyn Tool>)> {
    let mgr = manager.read().await;
    let all_tools = mgr.all_tools().await;
    drop(mgr);

    let mut tools = Vec::new();
    for (server_name, tool_def) in all_tools {
        let prefixed_name = dispatch_name(&server_name, &tool_def.name);
        let mcp_tool = McpTool::new(server_name, tool_def, Arc::clone(&manager));
        tools.push((prefixed_name, Arc::new(mcp_tool) as Arc<dyn Tool>));
    }
    tools
}

/// Build proxy tools for a single server from cached schemas, without requiring
/// a live connection. Used to advertise a server's tools immediately at spawn
/// (the proxy connects on first call). The returned tools are functionally
/// identical to live ones; only their definitions come from the disk cache.
pub fn create_mcp_tools_from_cached(
    server_name: &str,
    tool_defs: &[McpToolDef],
    manager: Arc<RwLock<McpManager>>,
) -> Vec<(String, Arc<dyn Tool>)> {
    tool_defs
        .iter()
        .map(|tool_def| {
            let prefixed_name = dispatch_name(server_name, &tool_def.name);
            let mcp_tool = McpTool::new(
                server_name.to_string(),
                tool_def.clone(),
                Arc::clone(&manager),
            );
            (prefixed_name, Arc::new(mcp_tool) as Arc<dyn Tool>)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{convert_content_blocks, dispatch_name};
    use super::super::protocol::ContentBlock;

    #[test]
    fn hyphenated_mcp_names_are_safe_for_the_standard_dispatcher() {
        assert_eq!(
            dispatch_name("context7", "resolve-library-id"),
            "mcp__context7__resolve_library_id"
        );
        assert_eq!(
            dispatch_name("hyphenated-server", "query-docs"),
            "mcp__hyphenated_server__query_docs"
        );
    }

    #[test]
    fn image_blocks_are_routed_into_the_image_channel_not_stringified_away() {
        // An MCP tool result carrying a text block and an image block (the exact
        // shape the Mattermost MCP server returns for a screenshot attachment).
        let blocks = vec![
            ContentBlock::Text {
                text: "here is the screenshot".to_string(),
            },
            ContentBlock::Image {
                data: "aGVsbG8=".to_string(), // base64 for "hello"
                mime_type: "image/png".to_string(),
            },
        ];

        let (output, images) = convert_content_blocks(blocks);

        // The pixels reach the model: exactly one image block on the vec, with
        // the right media type and the base64 payload preserved verbatim.
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, "aGVsbG8=");
        // The old behavior stringified the image into the text and dropped the
        // pixels; assert the text is NOT the sole carrier - the breadcrumb marks
        // it as attached, and the real text block still comes through.
        assert!(output.contains("here is the screenshot"));
        assert!(output.contains("attached to model"));
    }

    #[test]
    fn non_image_results_carry_no_images() {
        let blocks = vec![ContentBlock::Text {
            text: "plain text only".to_string(),
        }];
        let (output, images) = convert_content_blocks(blocks);
        assert!(images.is_empty());
        assert_eq!(output, "plain text only");
    }
}
