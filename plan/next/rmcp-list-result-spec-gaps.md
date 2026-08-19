# rmcp negotiates protocol revisions it does not fully satisfy

> **Evidence for `plan/todo/0124-narrow-or-freeze-the-low-level-surfaces.md`.** This entry is the
> concrete measurement of what an rmcp major costs outrig, which is the input to 0124's verdict on
> whether `ProxyServer`'s rmcp coupling is frozen as supported 0.2.x surface or narrowed. The
> upstream issue and the three unhandled list methods stay here as work in their own right.

`rmcp` 3.1.0 lists every revision it knows in `ProtocolVersion::KNOWN_VERSIONS`, and
`ServerHandler::supported_protocol_versions` defaults to exactly that list. A server that does not
override it therefore agrees to speak whatever the SDK knows -- including revisions whose *required*
response fields rmcp's own constructors leave unset.

That is not hypothetical. `2026-07-28` adopted SEP-2549, which makes `ttlMs` and `cacheScope`
mandatory on paginated list results, but `ListToolsResult::with_all_items` leaves both `None` and
both are `skip_serializing_if = "Option::is_none"`. Upgrading rmcp 2.2 -> 3.1 (`9cd2b83b`) silently
moved outrig onto that revision, and every conforming client -- Claude Code among them -- began
rejecting `tools/list` outright and loading zero tools. No outrig source changed; the ceiling moved
underneath it. Fixed by emitting the fields and pinning
`outrig::mcp_proxy::SUPPORTED_PROTOCOL_VERSIONS`.

Two things worth doing:

1. **Upstream it.** rmcp should either populate the fields a negotiated revision requires, or
   default `supported_protocol_versions` to the revisions it can actually serve rather than to
   every revision it can name. Worth an issue against `modelcontextprotocol/rust-sdk`; the fix
   here is a workaround for a gap other rmcp servers will hit identically.

2. **The other list methods are already answering, and already malformed.** The same
   `paginated_result!` macro backs `ListResourcesResult`, `ListResourceTemplatesResult`, and
   `ListPromptsResult`, and rmcp dispatches all three to default `ServerHandler` bodies that
   return `List*Result::default()`. Advertised capabilities do not gate dispatch, so although
   both servers declare `tools` only, a client that asks anyway gets a *successful* empty result
   carrying `resultType` but neither `ttlMs` nor `cacheScope` -- the identical malformed shape
   that broke `tools/list`. Measured against `outrig mcp self` on `2026-07-28`:

   ```
   {"id":2,"result":{"resultType":"complete","resources":[]}}
   {"id":3,"result":{"resultType":"complete","prompts":[]}}
   {"id":4,"result":{"resultType":"complete","resourceTemplates":[]}}
   ```

   A capability-respecting client never asks, which is why this hurt nobody yet. The fix is
   probably not to fill in cache metadata for lists that do not exist, but to override the three
   methods on both handlers to return `method_not_found`, matching the tools-only capability set
   both servers actually advertise. Whoever adds resources or prompts for real inherits the
   original gap on top, and the unit tests added for `tools/list` will not catch either.

`SUPPORTED_PROTOCOL_VERSIONS` needs review on every rmcp upgrade: adding an entry is an assertion
that both servers meet that revision's requirements. The regression test in
`crates/outrig-cli/tests/mcp_self.rs` speaks raw JSON-RPC precisely because a typed rmcp client
deserializes the fields into `Option` and cannot see them missing.
