# Rich tool results for outrig's own agent loop

## Context

0002-43 made MCP tool results canonical data: `McpToolResult` carries the server's content
blocks in order, and a client connected to `outrig mcp` receives them intact. outrig's *own*
agent loop does not. `McpToolAdapter` (`crates/outrig-cli/src/rig_tool.rs`) implements `rig`'s
`ToolDyn::call`, whose return type is `String`, so `adapt_tool_result` reduces the blocks to
`McpToolResult::render_text()` and the model sees `[image: image/png, 4096 base64 bytes]`
where an image was.

Every provider outrig speaks to can accept image content in a tool result. The limit is the
`rig` seam, not the protocol and no longer outrig's own types.

## Goal

An image or audio block returned by an MCP tool reaches the model as that block, on providers
that accept one, without the agent loop losing the text-only path for providers that do not.

## Sketch

- Establish whether `rig` can carry non-text tool results at all, and at which version. If
  `ToolDyn::call -> String` is still the seam, the options are a `rig` upstream change, a
  parallel path that bypasses `ToolDyn` for multimodal results, or carrying the blocks
  out-of-band and splicing them into the message before the turn is sent.
- Decide per provider. The native Anthropic provider from 0002-21 is the one outrig controls
  end to end and is the natural first target.
- `tool-result-max` currently caps the rendered text. A blocks-carrying path needs its own
  answer for size, since base64 image payloads dwarf the text cap.
- Keep the rendering as the fallback, unchanged, so a provider that cannot take blocks behaves
  exactly as it does today.

## See also

- `crates/outrig-cli/src/rig_tool.rs` -- `adapt_tool_result`, the reduction.
- `doc/usage/mcp.md#tool-results` -- the asymmetry, as currently documented.
- `plan/done/phase/0002-sidecars/tasks/0002-43-mcp-content-is-canonical-not-rendered.md`
  -- decision 8.
