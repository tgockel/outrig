//! One tool as the agent loop carries it, whatever backs it.
//!
//! A session's tools come from two places: MCP servers running in the
//! container ([`McpToolAdapter`](crate::rig_tool::McpToolAdapter)) and
//! OutRig's own host-side built-ins ([`crate::builtin_tool`]). Both are
//! [`OutrigTool`] implementations, so [`SessionTool`] erases the difference --
//! once, into the [`DynamicTool`] rig's agent takes -- and the rest of the CLI
//! handles one list.
//!
//! The wrapper exists because the list has to be *cloned*, not just borrowed:
//! [`RebuildingAgent`](crate::llm::RebuildingAgent) keeps the tools and hands
//! Rig a fresh `Vec<DynamicTool>` every time it rebuilds. A `DynamicTool`
//! clones by sharing the tool behind it rather than duplicating its state --
//! which matters for MCP adapters, whose `Arc<McpClient>` is a live
//! connection, and for built-ins, which share the subagent registry.
//!
//! The definition already carries the name and description, so the banner
//! and `/tools` render straight off it and need no separate metadata.

use std::sync::{Arc, Mutex};

use rig::completion::message::ToolName;
use rig::tool::{DynamicTool, ToolContext, ToolExecutionError, ToolOutput};
use serde_json::Value;

use crate::rig_tool::McpToolAdapter;

/// A tool of OutRig's own, as the agent loop calls it.
///
/// Every name here is decided at runtime -- an MCP server's listing, or
/// `sanitize_tool_name` over a built-in's -- which is why this is not rig's
/// own [`Tool`](rig::tool::Tool), whose name is a `const`. The arguments
/// arrive decoded: rig parses the model's JSON, and a call whose arguments
/// are no object never reaches a tool.
///
/// An error reaches the model as its text, so a tool says why it failed in
/// words the model can act on ([`ToolExecutionError::new`]); rig's
/// `from_error` would redact them to "the tool failed".
pub trait OutrigTool: Send + Sync + 'static {
    /// What the model calls the tool.
    fn name(&self) -> String;

    /// What the model is told the tool does.
    fn description(&self) -> String;

    /// The JSON Schema of the arguments.
    fn parameters(&self) -> Value;

    /// Run the tool on `args`. `context` is the dispatch's, which carries the
    /// turn's [`TurnUndo`](crate::llm::TurnUndo) to the one tool that reads it.
    fn call(
        &self,
        context: &mut ToolContext,
        args: Value,
    ) -> impl Future<Output = Result<ToolOutput, ToolExecutionError>> + Send;
}

/// A reference-counted handle to one tool the agent can call.
#[derive(Clone)]
pub struct SessionTool(DynamicTool);

impl SessionTool {
    pub fn new<T: OutrigTool>(tool: T) -> Self {
        let tool = Arc::new(tool);
        // Every name comes through `sanitize_tool_name` or an MCP server's
        // listing, and neither yields an empty one.
        let name = ToolName::new(tool.name()).expect("a session tool has a name");
        let description = tool.description();
        let parameters = tool.parameters();
        Self(DynamicTool::new_with_context(
            name,
            description,
            parameters,
            move |context: &mut ToolContext, args: Value| {
                let tool = Arc::clone(&tool);
                Box::pin(async move { tool.call(context, args).await })
            },
        ))
    }

    /// What the model calls the tool.
    pub fn name(&self) -> &str {
        self.0.name().as_str()
    }

    /// What the model is told the tool does.
    pub fn description(&self) -> String {
        self.0.definition().description
    }

    /// The JSON Schema of the arguments.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn parameters(&self) -> Value {
        self.0.definition().parameters
    }

    /// Run the tool on `args` outside a turn, as a test or a smoke check does.
    #[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
    pub async fn run(&self, args: Value) -> Result<ToolOutput, ToolExecutionError> {
        self.0.run(args).await
    }
}

impl std::fmt::Debug for SessionTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SessionTool").field(&self.name()).finish()
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
pub fn erase<T: OutrigTool>(tools: impl IntoIterator<Item = T>) -> Vec<SessionTool> {
    tools.into_iter().map(SessionTool::new).collect()
}

/// Hand Rig the owned form its `AgentBuilder` wants.
pub fn dynamic(tools: &[SessionTool]) -> Vec<DynamicTool> {
    tools.iter().map(|tool| tool.0.clone()).collect()
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
