//! MCP -> Rig dynamic-tool adapter.
//!
//! [`McpToolAdapter`] wraps an MCP-discovered tool as a [`rig::tool::ToolDyn`]
//! so the agent loop can hand it directly to a Rig `Agent`. The original
//! tool name (used on the MCP wire) lives next to the sanitized
//! `<server>__<tool>` name (the form the LLM sees, produced by
//! [`outrig::sanitize_tool_name`]), so a single adapter knows both ends of
//! the dispatch.

use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::Value;

use crate::error::Result;
use outrig::{McpClient, McpToolResult};

/// A Rig dynamic-tool view of one MCP-discovered tool.
///
/// `openai_name` is what the LLM sees and what
/// [`ToolDyn::name`] returns. `mcp_tool_name` is the original name from
/// `tools/list`, used on the wire when dispatching back into the MCP server.
/// `client` is shared (via `Arc`) so multiple adapters fronting the same MCP
/// server reuse one connection.
#[derive(Debug, Clone)]
pub struct McpToolAdapter {
    pub openai_name: String,
    pub mcp_tool_name: String,
    pub description: String,
    pub input_schema: Value,
    pub result_cap_bytes: usize,
    pub client: Arc<McpClient>,
}

impl McpToolAdapter {
    /// Build one adapter per tool the server advertises. The server's local
    /// name (from [`McpClient::name`]) becomes the prefix.
    pub async fn from_client_tools(
        client: Arc<McpClient>,
        result_cap_bytes: usize,
    ) -> Result<Vec<McpToolAdapter>> {
        let tools = client.list_tools().await?;
        let server_name = client.name().to_string();
        Ok(tools
            .into_iter()
            .map(|t| McpToolAdapter {
                openai_name: outrig::sanitize_tool_name(&server_name, &t.name),
                mcp_tool_name: t.name,
                description: t.description.unwrap_or_default(),
                input_schema: t.input_schema,
                result_cap_bytes,
                client: client.clone(),
            })
            .collect())
    }
}

/// Error wrapper that carries an MCP tool-call failure into rig's
/// `ToolError::ToolCallError(Box<dyn Error>)` channel without leaking our
/// concrete `OutrigError` type into rig's API.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct McpAdapterError(String);

pub fn truncate_for_llm(result: &str, cap: usize) -> String {
    if result.is_empty() || result.len() <= cap {
        return result.to_string();
    }
    if cap == 0 {
        return String::new();
    }

    let original_len = result.len();
    let mut cut = cap.saturating_sub(truncation_marker(original_len, cap, 0).len());
    loop {
        cut = floor_char_boundary(result, cut.min(result.len()));
        let marker = truncation_marker(original_len, cap, cut);
        if marker.len() >= cap {
            return truncate_marker(&marker, cap);
        }

        let content_budget = cap - marker.len();
        if cut <= content_budget {
            let mut truncated = String::with_capacity(cut + marker.len());
            truncated.push_str(&result[..cut]);
            truncated.push_str(&marker);
            debug_assert!(truncated.len() <= cap);
            return truncated;
        }

        cut = content_budget;
    }
}

fn adapt_tool_result(result: McpToolResult, cap: usize) -> std::result::Result<String, ToolError> {
    let content_text = truncate_for_llm(&result.content_text, cap);
    if result.is_error {
        Err(ToolError::ToolCallError(Box::new(McpAdapterError(
            content_text,
        ))))
    } else {
        Ok(content_text)
    }
}

fn truncation_marker(original_len: usize, cap: usize, kept: usize) -> String {
    let dropped = original_len.saturating_sub(kept);
    format!(
        concat!(
            "\n\n[outrig: tool result truncated]\n",
            "  original size: {original_len} bytes\n",
            "  cap:           {cap} bytes\n",
            "  kept:          first {kept} bytes; trailing {dropped} bytes dropped.\n\n",
            "  This tool result was larger than the configured cap. Your next call\n",
            "  should narrow the query: use head/tail/grep/--max-count, scope a\n",
            "  directory or line range, or call a more specific tool. Re-running\n",
            "  the same call will produce the same truncation.",
        ),
        original_len = original_len,
        cap = cap,
        kept = kept,
        dropped = dropped,
    )
}

fn truncate_marker(marker: &str, cap: usize) -> String {
    let cut = floor_char_boundary(marker, cap.min(marker.len()));
    marker[..cut].to_string()
}

fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    while !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

impl ToolDyn for McpToolAdapter {
    fn name(&self) -> String {
        self.openai_name.clone()
    }

    fn definition(&self, _prompt: String) -> WasmBoxedFuture<'_, ToolDefinition> {
        Box::pin(async move {
            ToolDefinition {
                name: self.openai_name.clone(),
                description: self.description.clone(),
                parameters: self.input_schema.clone(),
            }
        })
    }

    fn call(&self, args: String) -> WasmBoxedFuture<'_, std::result::Result<String, ToolError>> {
        Box::pin(async move {
            let parsed: Value = if args.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&args)?
            };

            let result = self
                .client
                .call_tool(&self.mcp_tool_name, parsed)
                .await
                .map_err(|e| ToolError::ToolCallError(Box::new(McpAdapterError(e.to_string()))))?;

            adapt_tool_result(result, self.result_cap_bytes)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 1024;

    #[test]
    fn truncate_for_llm_leaves_empty_and_under_cap_results_unchanged() {
        assert_eq!(truncate_for_llm("", CAP), "");
        assert_eq!(truncate_for_llm("short", CAP), "short");
    }

    #[test]
    fn truncate_for_llm_leaves_exact_cap_result_unchanged() {
        let input = "a".repeat(CAP);

        let output = truncate_for_llm(&input, CAP);

        assert_eq!(output, input);
    }

    #[test]
    fn truncate_for_llm_caps_one_byte_over_with_marker() {
        let input = "a".repeat(CAP + 1);

        let output = truncate_for_llm(&input, CAP);

        assert!(output.len() <= CAP, "output len: {}", output.len());
        assert!(output.contains("[outrig: tool result truncated]"));
        assert!(output.contains("original size: 1025 bytes"));
        assert!(output.contains("cap:           1024 bytes"));
        assert!(output.ends_with("produce the same truncation."));
    }

    #[test]
    fn truncate_for_llm_caps_large_result_with_original_size_and_hint() {
        let input = "x".repeat(5 * 1024 * 1024);

        let output = truncate_for_llm(&input, 4096);

        assert!(output.len() <= 4096, "output len: {}", output.len());
        assert!(output.contains("original size: 5242880 bytes"));
        assert!(output.contains("cap:           4096 bytes"));
        assert!(output.contains("should narrow the query"));
    }

    #[test]
    fn truncate_for_llm_keeps_valid_utf8_at_boundary() {
        let input = format!("{}{}", "a".repeat(900), "🙂".repeat(200));

        let output = truncate_for_llm(&input, CAP);

        assert!(output.len() <= CAP, "output len: {}", output.len());
        assert!(output.is_char_boundary(output.len()));
        assert!(output.contains("[outrig: tool result truncated]"));
    }

    #[test]
    fn adapt_tool_result_truncates_success_content() {
        let result = McpToolResult {
            content_text: "a".repeat(CAP + 1),
            is_error: false,
        };

        let output = adapt_tool_result(result, CAP).expect("success result");

        assert!(output.len() <= CAP, "output len: {}", output.len());
        assert!(output.contains("[outrig: tool result truncated]"));
    }

    #[test]
    fn adapt_tool_result_truncates_error_content() {
        let result = McpToolResult {
            content_text: "e".repeat(CAP + 1),
            is_error: true,
        };

        let err = adapt_tool_result(result, CAP).expect_err("error result");
        let ToolError::ToolCallError(source) = err else {
            panic!("expected tool-call error");
        };
        let msg = source.to_string();

        assert!(msg.len() <= CAP, "error len: {}", msg.len());
        assert!(msg.contains("[outrig: tool result truncated]"));
    }
}
