//! One tool as the agent loop carries it, whatever backs it.
//!
//! A session's tools come from two places: MCP servers running in the
//! container ([`McpToolAdapter`](crate::rig_tool::McpToolAdapter)) and
//! OutRig's own host-side built-ins ([`crate::builtin_tool`]). Both are
//! [`ToolDyn`] implementations, so [`SessionTool`] erases the difference and
//! the rest of the CLI handles one list.
//!
//! The wrapper exists because the list has to be *cloned*, not just borrowed:
//! [`RebuildingAgent`](crate::llm::RebuildingAgent) keeps the tools and hands
//! Rig a fresh `Vec<Box<dyn ToolDyn>>` every time it rebuilds, and a
//! `Box<dyn ToolDyn>` is not `Clone`. An `Arc` is, so cloning the list shares
//! the tools rather than duplicating their state -- which matters for MCP
//! adapters, whose `Arc<McpClient>` is a live connection, and for built-ins,
//! which share the subagent registry.
//!
//! `ToolDyn` already exposes `name()` and `description()`, so the banner and
//! `/tools` render straight off the trait and need no separate metadata.

use std::sync::Arc;

use rig::tool::{ToolCallExtensions, ToolDyn, ToolError, ToolExecutionResult};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::Value;

/// A reference-counted handle to one tool the agent can call.
#[derive(Clone)]
pub struct SessionTool(Arc<dyn ToolDyn>);

impl SessionTool {
    pub fn new<T: ToolDyn + 'static>(tool: T) -> Self {
        Self(Arc::new(tool))
    }
}

impl std::fmt::Debug for SessionTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionTool").field(&self.0.name()).finish()
    }
}

/// Delegates every method rather than only `call`, so a built-in that
/// overrides `call_structured` to report a precise [`ToolOutcome`] keeps that
/// behavior through the wrapper instead of silently falling back to the
/// blanket default.
///
/// [`ToolOutcome`]: rig::tool::ToolOutcome
impl ToolDyn for SessionTool {
    fn name(&self) -> String {
        self.0.name()
    }

    fn description(&self) -> String {
        self.0.description()
    }

    fn parameters(&self) -> Value {
        self.0.parameters()
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        self.0.call(args)
    }

    fn call_with_extensions<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        self.0.call_with_extensions(args, extensions)
    }

    fn call_structured<'a>(
        &'a self,
        args: String,
        extensions: &'a ToolCallExtensions,
    ) -> WasmBoxedFuture<'a, ToolExecutionResult> {
        self.0.call_structured(args, extensions)
    }
}

/// Erase a list of concrete tools into the shared form.
pub fn erase<T: ToolDyn + 'static>(tools: impl IntoIterator<Item = T>) -> Vec<SessionTool> {
    tools.into_iter().map(SessionTool::new).collect()
}

/// Hand Rig the owned, boxed form its `AgentBuilder` wants.
pub fn boxed(tools: &[SessionTool]) -> Vec<Box<dyn ToolDyn>> {
    tools
        .iter()
        .map(|t| Box::new(t.clone()) as Box<dyn ToolDyn>)
        .collect()
}
