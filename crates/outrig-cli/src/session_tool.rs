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

use std::sync::{Arc, Mutex};

use rig::tool::{ToolCallExtensions, ToolDyn, ToolError, ToolExecutionResult};
use rig::wasm_compat::WasmBoxedFuture;
use serde_json::Value;

use crate::rig_tool::McpToolAdapter;

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

/// A tool list several holders share and one of them grows: the session's MCP
/// tools, which the primary agent extends when `/sidecar add` connects a
/// sidecar and which every subagent registry launches from, nested ones
/// included. A reader takes a [`snapshot`](Self::snapshot) and keeps it, so a
/// tool added afterwards reaches the next reader rather than that one.
///
/// A `Mutex` rather than a `RefCell` because subagents launch from rounds
/// running on spawned tasks. The lock is never held across an await.
#[derive(Clone, Default)]
pub struct SharedTools(Arc<Mutex<Vec<SessionTool>>>);

impl SharedTools {
    pub fn new(tools: Vec<SessionTool>) -> Self {
        Self(Arc::new(Mutex::new(tools)))
    }

    /// The list as it stands now.
    pub fn snapshot(&self) -> Vec<SessionTool> {
        self.lock().clone()
    }

    /// Append `tools`; every snapshot taken from here on carries them.
    pub fn extend(&self, tools: impl IntoIterator<Item = SessionTool>) {
        self.lock().extend(tools);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<SessionTool>> {
        self.0.lock().expect("shared tool list poisoned")
    }
}

/// Give every tool its advertised name through [`outrig::assign_tool_names`],
/// the rule `outrig mcp` applies, and drop any tool left without one. `identity`
/// is the tool's `(server, tool)` pair as the upstream knows it, which is
/// gone once the tool is erased into a [`SessionTool`], so this runs before.
///
/// `reserved` are names that cannot move: the built-ins, and on `/sidecar add`
/// every name the model has already seen. A tool landing on one is the one
/// widened, so unlike `outrig mcp` the outcome there can depend on arrival
/// rather than on the canonical `(server, tool)` order.
pub fn assign_names<T>(
    tools: Vec<T>,
    identity: impl Fn(&T) -> (&str, &str),
    rename: impl Fn(&mut T, String),
    reserved: &[String],
) -> Vec<T> {
    let identities: Vec<(&str, &str)> = tools.iter().map(&identity).collect();
    let reserved: Vec<&str> = reserved.iter().map(String::as_str).collect();
    let names = outrig::assign_tool_names(&identities, &reserved);
    tools
        .into_iter()
        .zip(names)
        .filter_map(|(mut tool, name)| {
            rename(&mut tool, name?);
            Some(tool)
        })
        .collect()
}

/// [`assign_names`] over MCP adapters, identified by their server and the
/// upstream tool name.
pub fn name_adapters(adapters: Vec<McpToolAdapter>, reserved: &[String]) -> Vec<McpToolAdapter> {
    assign_names(
        adapters,
        |a| (a.client.name(), a.mcp_tool_name.as_str()),
        |a, name| a.openai_name = name,
        reserved,
    )
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

#[cfg(test)]
mod name_tests {
    use super::*;

    /// A `(server, tool)` identity and the name it ends up advertised under.
    type Fake = ((&'static str, &'static str), String);

    fn name(tools: Vec<(&'static str, &'static str)>, reserved: &[&str]) -> Vec<Fake> {
        let reserved: Vec<String> = reserved.iter().map(|s| s.to_string()).collect();
        assign_names(
            tools.into_iter().map(|id| (id, String::new())).collect(),
            |t: &Fake| t.0,
            |t, name| t.1 = name,
            &reserved,
        )
    }

    fn names(named: &[Fake]) -> Vec<&str> {
        named.iter().map(|t| t.1.as_str()).collect()
    }

    fn distinct(names: &[&str]) -> bool {
        let set: std::collections::HashSet<_> = names.iter().collect();
        set.len() == names.len()
    }

    #[test]
    fn a_literal_suffixed_name_does_not_replace_its_lookalike() {
        let named = name(vec![("fs", "read/file"), ("fs", "read_file_5d2270")], &[]);
        assert_eq!(named.len(), 2);
        assert!(distinct(&names(&named)));
    }

    #[test]
    fn one_name_listed_twice_keeps_both() {
        let named = name(vec![("fs", "echo"), ("fs", "echo")], &[]);
        assert_eq!(named.len(), 2);
        assert!(distinct(&names(&named)));
    }

    #[test]
    fn a_reserved_name_is_kept_and_the_tool_moves() {
        let named = name(vec![("fs", "echo")], &["fs__echo"]);
        assert_eq!(named.len(), 1);
        assert_ne!(named[0].1, "fs__echo");
    }
}
