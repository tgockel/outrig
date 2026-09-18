# 0002-43 -- MCP content is canonical data, or the reduced contract is written down

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
  Neutral matters: the blocks must not be rmcp types, or this task hands 0002-47 a worse problem.
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

   The queue runs one task end to end at a time, so a prose instruction to "coordinate with 0002-47"
   is not executable: 0002-43 would freeze a representation before 0002-47 ever reaches its verdict.
   So the *boundary decision* lands here, in this task's `## Decisions` -- content blocks, and
   the principle governing errors and protocol versions -- and 0002-47 takes it as a hard dependency
   and applies it to the surfaces this task does not touch. This task must therefore look at
   `OutrigError`'s three rmcp variants and `SUPPORTED_PROTOCOL_VERSIONS` before deciding, even
   though it changes neither: a content-only answer that implies a different answer for those is
   the failure being avoided.

## Dependencies

None hard. Must land before 0002-48 regenerates the snapshot, and its fork-2 answer must be reached
jointly with 0002-47's fork 1.

## See also

- `crates/outrig/src/mcp.rs` -- `McpTool` (34), `McpToolResult` (54), `list_tools` (262),
  `call_tool` (288), the conversion (305-350).
- `crates/outrig/src/mcp_proxy.rs` -- `list_tools_inner` (292), `dispatch_call` (318);
  `crates/outrig/src/mcp_proxy_dispatch_tests.rs` -- the tests whose fake cannot express the loss.

## Decisions

1. **Fork 1: lossless, as recommended.** `McpToolResult` carries `Vec<McpContent>`,
   `structured_content`, and result-level `meta`; `McpTool` carries the whole descriptor. The
   counter-argument was scope, and the task is indeed the largest of the pre-freeze set, but
   the reduction was never a decision anyone made and freezing it would have made the repair
   semantically disruptive rather than merely breaking.

2. **Fork 2 -- the rmcp boundary. This is the answer 0002-47 inherits.**

   > rmcp types are permitted on outrig's public surface **only where the item exists to
   > participate in rmcp's own machinery** -- implementing an rmcp trait, or being handed
   > straight back to rmcp. Wherever a value carries information outrig reports in its own
   > right -- tool descriptors, tool results, errors -- the type is outrig's.

   The test is *why the item exists*, not whether an rmcp type appears in it. Applied:

   | Surface                                                     | Verdict                    |
   |-------------------------------------------------------------|----------------------------|
   | content blocks, tool descriptors                             | outrig-owned (done here)   |
   | `ProxyServer`'s `ServerHandler` impl                         | rmcp, frozen               |
   | `dispatch_call`, `list_tools_inner`                          | rmcp, frozen               |
   | `SUPPORTED_PROTOCOL_VERSIONS`                                | rmcp, frozen               |
   | `OutrigError`'s three rmcp variants, `From<ServerInitializeError>` | outrig-owned -- 0002-47 |

   `dispatch_call` and `list_tools_inner` are the `RequestContext`-free halves of the server
   impl and exist only to be its testable core, so they inherit its verdict. The version
   constant exists to be returned from `supported_protocol_versions`; it is an argument to
   rmcp, not a fact outrig reports. The error variants are the opposite: they are reachable
   from every fallible call in the crate and are what a caller matches on, so the principle
   says narrow them. Checking the principle against those three before committing to it was
   the point of the fork; it gives a different answer for the error variants than for the
   proxy, and that asymmetry is the principle working rather than failing.

   The conversions are `pub(crate)` free functions rather than `From` impls, since a public
   `From<CallToolResult>` would put rmcp straight back on the surface.

3. **`content_text` becomes `render_text()`, not a same-named accessor.** A method spelled
   like the old field would let a call site keep compiling while its meaning changed from "the
   result" to "one view of the result". The rename makes every call site a compile error whose
   fix is mechanical, and the rendering itself is byte-identical for text-only results, so no
   transcript shifts.

4. **Unknown block kinds round-trip opaquely, and are named in the rendering.** rmcp's
   `ContentBlock` is `#[serde(tag = "type")]` with no catch-all, so a kind *rmcp* does not know
   fails to decode a layer below outrig and never arrives. The reachable case is a kind rmcp
   learns and outrig has not: captured as `McpContent::Other { kind, raw }` from the block's
   own serialization and rebuilt with `from_value` on the way out, so the proxy is not what
   drops it. If the rebuild fails it degrades to `[unsupported content block: {kind}]` as text
   rather than erroring -- losing one block beats losing the result. `McpResourceContents::Other`
   works the same way.

5. **`_meta` and `structured_content` are preserved; `resultType` is not.** The first two are
   carried in both directions, at result level and per block, asserted directly. `resultType`
   is normalized to `complete`: `task` and `input_required` promise `tasks/*` and elicitation
   follow-ups `ProxyServer` does not implement, and relaying the marker without the methods
   advertises a surface that is not there. This is also what `CallToolResult::success`/`::error`
   already did, so the change is that it is now a decision with a test rather than a default.

6. **Foreign enums are mirrored by their declared stability.** `Role` is declared exhaustive by
   both the spec and rmcp, so `McpRole` is a real two-variant enum a consumer can match without
   a wildcard. `IconTheme` is `#[non_exhaustive]`, so `McpIcon::theme` is the wire `String`: a
   mirror of a growable foreign enum needs an escape hatch anyway, and a string *is* the escape
   hatch. A theme string rmcp cannot name is dropped on the outbound leg rather than guessed at.

7. **`ToolHandle` is fixed here, outside the four legs.** It is the `Outrig` facade's own
   flattened descriptor, and 0.2.0 freezes it too; leaving it would have frozen the identical
   defect one tier up, for the sake of a scope line. It is built directly from `McpTool`, so
   the change is the five fields and nothing else.

8. **The two exits keep different fidelity, on purpose.** A client reaching outrig through
   `outrig mcp` receives the blocks; the agent loop inside `outrig run` receives
   `render_text()`, because `rig`'s `ToolDyn::call` returns `String`. Documented in
   `doc/usage/mcp.md` rather than left to be discovered, and the remaining half is
   `plan/next/rich-tool-results-for-the-agent-loop.md`.

9. **The end-to-end compares the payload, not the envelope.** rmcp strips `resultType` when the
   peer negotiated a revision that predates it (`handler/server.rs`'s
   `strip_result_type_for_legacy_peer`), so a whole-value equality assertion across a real
   client would be asserting rmcp's downgrade behavior rather than outrig's forwarding.

10. **The advertised `Tool` is assembled once, at `ProxyServer::build`.** The first cut kept the
    whole `McpTool` per entry and rebuilt the rmcp `Tool` on every `tools/list`, which deep-cloned
    the output schema into a fresh `Arc` each time -- and left `ToolEntry` holding the input schema
    twice, once as the `Value` inside `McpTool` and once as the validated `Arc<JsonObject>` beside
    it. The table is frozen for the life of the proxy, so a listing is a clone of a finished
    answer: `ToolEntry` is `{ listed: Tool, backend_tool, client_idx }` and nothing is derived per
    request. The converters stay pure functions, so the per-leg tests are unaffected.

    What that leaves is `ToolHandle` still flattening `McpTool` field by field, and
    `McpTool::input_schema` still typed `Value` when the protocol requires an object. Both are
    public-surface breaks on types this task did not set out to redesign, and both are
    `plan/next/tool-descriptor-is-shaped-twice.md`.
