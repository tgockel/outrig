# The tool descriptor is shaped twice, and one of its fields is typed loose

## Context

0002-43 made `McpTool` carry the whole upstream descriptor. `ToolHandle` -- the `Outrig`
facade's own tool listing (`crates/outrig/src/outrig_.rs`) -- is `McpTool` plus a `server`
field, with `description` flattened from `Option<String>` to `String`, and `tool_handles`
copies all eight fields across by hand. The two structs were grown in lockstep by 0002-43, and
nothing would catch a ninth field added to one and not the other: it would silently stop
propagating.

Separately, `McpTool::input_schema` is a `serde_json::Value` whose own doc comment says MCP
requires an object -- `ProxyServer::build` is what rejects anything else. A field typed one
level looser than what is actually required forces the validation, and the `Arc<JsonObject>`
it produces, to live outside the type: `tool_to_rmcp` takes the validated schema as a third
parameter rather than reading it off the `&McpTool` it is already given.

Both were raised during 0002-43's `/simplify` pass and deliberately left: each converts an
additive change into a breaking one on a type the task did not set out to redesign.

## Goal

The facade's descriptor and the client's descriptor cannot drift, and `input_schema`'s type
says what the protocol requires.

## Sketch

- `ToolHandle { server: String, tool: McpTool }`, with `description()` as a one-line accessor
  if the non-optional convenience is worth keeping. Or ask the prior question: whether the
  facade needs a descriptor type of its own at all, rather than handing back
  `(&str, &McpTool)`. That is the real fork.
- `McpTool::input_schema: Arc<JsonObject>` (or `JsonObject`), which folds the validation into
  construction and lets `tool_to_rmcp` drop its third parameter. Note the ripple:
  `ToolHandle::input_schema` is `Value` and is read by `crates/outrig-cli/src/rig_tool.rs` on
  its way into a `rig` tool definition.
- Both are public-surface breaks, so they want to land before a freeze rather than after --
  which makes this a 0.3 item unless it is pulled forward.

## See also

- `crates/outrig/src/outrig_.rs` -- `ToolHandle`, `tool_handles`.
- `crates/outrig/src/mcp_content.rs` -- `McpTool`, `tool_to_rmcp`.
- `plan/done/phase/0002-sidecars/tasks/0002-43-mcp-content-is-canonical-not-rendered.md`
  -- decision 7, which is why `ToolHandle` was touched at all.
