//! MCP -> Rig dynamic-tool adapter.
//!
//! [`McpToolAdapter`] wraps an MCP-discovered tool as a [`rig::tool::ToolDyn`]
//! so the agent loop can hand it directly to a Rig `Agent`. The original
//! tool name (used on the MCP wire) lives next to the sanitized
//! `<server>__<tool>` name (the form the LLM sees), so a single adapter knows
//! both ends of the dispatch.
//!
//! [`sanitize`] enforces OpenAI's `^[a-zA-Z0-9_-]{1,64}$` constraint on tool
//! names. Over-long names are truncated and tagged with a stable 6-hex blake3
//! suffix derived from the *pre-sanitization* `<server>__<tool>` so two tools
//! that would otherwise collide after character replacement get distinct
//! suffixes.

use std::sync::Arc;

use rig::completion::ToolDefinition;
use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::Value;

use crate::error::Result;
use crate::mcp::McpClient;

/// Maximum length OpenAI accepts for a tool name. Other providers are more
/// liberal but this is the safe lower bound.
const MAX_NAME_LEN: usize = 64;

/// Width of the truncation suffix's hex portion. The full suffix is
/// `_` + this many hex chars.
const HASH_HEX_LEN: usize = 6;
const SUFFIX_LEN: usize = 1 + HASH_HEX_LEN;

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
    pub client: Arc<McpClient>,
}

impl McpToolAdapter {
    /// Build one adapter per tool the server advertises. The server's local
    /// name (from [`McpClient::name`]) becomes the prefix.
    pub async fn from_client_tools(client: Arc<McpClient>) -> Result<Vec<McpToolAdapter>> {
        let tools = client.list_tools().await?;
        let server_name = client.name().to_string();
        Ok(tools
            .into_iter()
            .map(|t| McpToolAdapter {
                openai_name: sanitize(&server_name, &t.name),
                mcp_tool_name: t.name,
                description: t.description.unwrap_or_default(),
                input_schema: t.input_schema,
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

            if result.is_error {
                Err(ToolError::ToolCallError(Box::new(McpAdapterError(
                    result.content_text,
                ))))
            } else {
                Ok(result.content_text)
            }
        })
    }
}

/// Build the LLM-facing name from `<server>__<tool>`, replacing any character
/// outside `[a-zA-Z0-9_-]` with `_` and truncating with a stable 6-hex blake3
/// suffix when the result would exceed [`MAX_NAME_LEN`].
///
/// The hash is over the *pre-sanitization* concatenation, so two distinct
/// originals that would map to the same sanitized prefix get different
/// suffixes.
pub fn sanitize(server: &str, tool: &str) -> String {
    let original = format!("{server}__{tool}");

    let mut sanitized = String::with_capacity(original.len());
    for c in original.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }

    if sanitized.len() <= MAX_NAME_LEN {
        return sanitized;
    }

    let hash = blake3::hash(original.as_bytes());
    let hex = hash.to_hex();
    let suffix_hex = &hex.as_str()[..HASH_HEX_LEN];
    sanitized.truncate(MAX_NAME_LEN - SUFFIX_LEN);
    sanitized.push('_');
    sanitized.push_str(suffix_hex);
    sanitized
}
