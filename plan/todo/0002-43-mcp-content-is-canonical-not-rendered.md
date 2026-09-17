# 0120 -- MCP content is canonical data, or the reduced contract is written down

## Context

outrig's MCP types are a lossy projection of the protocol. `McpTool`
(`crates/outrig/src/mcp.rs:34`) keeps name, description, and input schema. `McpToolResult`
(`mcp.rs:54`) is:

```rust
pub struct McpToolResult {
    pub content_text: String,
    pub is_error: bool,
}
```

The loss happens at **four** places, in both directions, and a fix that covers one leg is not a
fix:

- **Upstream tools in** -- `McpClient::list_tools` (`mcp.rs:262`) drops output schema,
  annotations, icons, and metadata.
- **Upstream results in** -- `McpClient::call_tool` (`mcp.rs:288-350`) drops block type, payload,
  block boundaries, and structured content.
- **Proxied tools out** -- `ProxyServer::list_tools_inner` (`mcp_proxy.rs:292`) can only
  re-advertise what `McpTool` kept.
- **Proxied results out** -- `ProxyServer::dispatch_call` (`mcp_proxy.rs:318-341`) rebuilds a
  single `ContentBlock::text` from the flattened string.

The conversion at `mcp.rs:305-350` walks rmcp's ordered `ContentBlock` list and concatenates it.
Text passes through; everything else becomes a display placeholder -- `[blob: {mime}, N base64
bytes]`, `[resource link: {uri}]`, `[unsupported content block]`.

What is lost is **type, payload, metadata, block boundaries, and structured content**. Fragment
*order* is not lost -- the concatenation is in order -- so "order goes to die" was the wrong
rationale for this task and is corrected here. Losing boundaries is still severe: a client cannot
tell one block from the next, and cannot tell text that was text from text that was a placeholder.

The proxy tests all pass, and that is the problem worth naming: their fake can only manufacture
the reduced text type, so nothing in the suite exercises the loss. `mcp_proxy_dispatch_tests.rs`
tests dispatch, not fidelity.

Why it is a freeze item rather than an ordinary gap: adding fields later is source-additive,
because the structs are `#[non_exhaustive]`. But every consumer who ships against 0.2.0 will
build around `content_text` as *the* result model, because it is the only one there is. Repairing
that later is semantically disruptive even though it compiles.

## Goal

Either the canonical data is the protocol's data and text is a derived view, or the reduction is
a written contract that consumers can rely on deliberately rather than by default.

## Deliverables

Under the recommended fork:

- **`McpToolResult` carries ordered, protocol-neutral content blocks** plus structured content.
  Neutral matters: the blocks must not be rmcp types, or this task hands 0124 a worse problem.
- **`content_text` stops being a public mutable field.** Two sources of truth that can disagree
  is a worse API than one lossy one. It becomes a derived accessor or an explicit renderer; keep
  `McpToolResult::ok(text)` / `::error(text)` as convenience constructors that build a single
  text block, since those are the ergonomic reason the field existed.
- **`McpTool` keeps the richer descriptor** -- output schema, annotations, icons, metadata -- so
  a proxied tool advertises what its upstream advertises.
- **All four legs are fixed and each is testable on its own.** Extract the conversions so
  `McpClient`'s two directions can be exercised without a live server; growing only the proxy
  fake lets the planned round trip pass while `McpClient` discards the data first.
- **Unknown future block types have a stated behavior.** rmcp will add variants; whether they
  round-trip opaquely, render as a named placeholder, or error is a contract decision, not a
  `_ =>` arm.
- **Result-level `_meta` and protocol extension fields get an explicit disposition.** They sit
  beside the content list rather than inside it, so a fix that preserves blocks can still drop
  them silently. Preserve them, or freeze the omission in writing -- either way with a test, so
  the choice is visible rather than incidental.
- **The rendering rule is documented**, since the derived text remains what a model sees:
  `doc/usage/mcp.md` and `doc/reference/config.md` (a **symlink** into
  `crates/outrig-cli/src/mcp_self/docs/` -- edit the target).
- `crates/outrig/public-api.txt` regenerated; `crates/outrig/CHANGELOG.md` states plainly which
  fidelity a 0.2.x consumer may depend on.

Under the alternative fork, the deliverable is the sentence instead: docs and changelog say
`outrig mcp` presents a text-only projection, name the blocks that become placeholders, and say
that richer fidelity is a future minor rather than an oversight.

## Acceptance

- **Per-leg tests.** Each of the four conversions above is exercised directly: an rmcp result
  carrying an image block, an audio block, a blob, a resource link (both text and blob contents),
  and structured content survives `McpClient::call_tool`; the same survives
  `ProxyServer::dispatch_call`; the descriptor fields survive both tool legs.
- **End-to-end.** A fake upstream server returning a mixed result is observable, intact, by a
  client through `ProxyServer`.
- **Mixed rendering is pinned.** The derived text for a result containing text *and* non-text
  blocks has a golden expected value. Pinning only the text-only case leaves the interesting
  behavior unspecified.
- Block boundaries and order are both preserved, and a consumer can tell one block from the next.
- Result-level `_meta` behaves as the disposition above says, asserted directly.
- The derived text rendering is unchanged for text-only results -- no existing session's
  transcript shifts.
- `is_error` still mirrors the server's flag (`mcp.rs:347`).

## Design forks

1. **Lossless now, versus freezing the reduction -- Recommended: lossless.** The reduction is not
   a decision anyone made; it is what the first implementation happened to do, and shipping it as
   the frozen contract makes later repair semantically disruptive. The counter-argument is scope:
   this is the largest of the pre-freeze API tasks. If the reduction is kept, it must be kept *on
   purpose*, in writing.

2. **Where rmcp stops being an implementation detail -- Open, and this task owns the answer.**
   An outrig-owned `ContentBlock` enum is the honest shape and duplicates a chunk of the
   protocol. Re-exporting rmcp's is smaller and pulls rmcp deeper into the public surface.

   The queue runs one task end to end at a time, so a prose instruction to "coordinate with 0124"
   is not executable: 0120 would freeze a representation before 0124 ever reaches its verdict.
   So the *boundary decision* lands here, in this task's `## Decisions` -- content blocks, and
   the principle governing errors and protocol versions -- and 0124 takes it as a hard dependency
   and applies it to the surfaces this task does not touch. This task must therefore look at
   `OutrigError`'s three rmcp variants and `SUPPORTED_PROTOCOL_VERSIONS` before deciding, even
   though it changes neither: a content-only answer that implies a different answer for those is
   the failure being avoided.

## Dependencies

None hard. Must land before 0125 regenerates the snapshot, and its fork-2 answer must be reached
jointly with 0124's fork 1.

## See also

- `crates/outrig/src/mcp.rs` -- `McpTool` (34), `McpToolResult` (54), `list_tools` (262),
  `call_tool` (288), the conversion (305-350).
- `crates/outrig/src/mcp_proxy.rs` -- `list_tools_inner` (292), `dispatch_call` (318);
  `crates/outrig/src/mcp_proxy_dispatch_tests.rs` -- the tests whose fake cannot express the loss.
