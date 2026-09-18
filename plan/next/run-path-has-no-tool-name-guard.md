# `outrig run` advertises tool names with nothing checking them for duplicates

## Context

`0002-44` made every lossy tool name carry a hash suffix and taught `ProxyServer::build` to
widen a suffix rather than abort when two names still land on top of each other. That closes
the hazard on the `outrig mcp` side. The `outrig run` side has no guard at all.

`McpToolAdapter::from_client_tools` (`crates/outrig-cli/src/rig_tool.rs:39-56`) maps each
upstream tool through `outrig::sanitize_tool_name` into a `Vec` and returns it. The caller
concatenates those across servers without looking at what it already has
(`crates/outrig-cli/src/cli/run.rs:273-287`):

```rust
for arc in runtime.mcp_arcs.iter() {
    let adapters = McpToolAdapter::from_client_tools(arc.clone(), ...).await?;
    all_tools.extend(session_tool::erase(adapters));
}
```

The dynamic path that `add_sidecar` feeds (`cli/run.rs:810`) has the same shape, and the
built-in tools (`builtin_tool::name_of`) are added beside both without a check either. Two
adapters sharing an `openai_name` are handed to rig, which resolves a call by name -- so one
of them is unreachable and nothing says which. That is the "silently advertising one name for
two tools" case `0002-44` calls unacceptable, on the half of the product `0002-44` did not
reach.

`0002-44` shrank the exposure rather than closing it: the unsuffixed form is injective, so a
duplicate now requires a blake3 collision between two suffixed names. It did not remove it.

## Goal

`outrig run` notices two tools claiming one advertised name, on the same terms the proxy
does.

## Deliverables

- **One assembly point that checks.** The per-server loop and the dynamic-add path both
  extend one list; whichever helper they share should reject or re-derive a repeat rather than
  appending it. `SessionTool` is type-erased, so this needs a name accessor on it.
- **Match the proxy's policy, or say why not.** `ProxyServer::build_with_widths` widens the
  loser's suffix and logs both identities at ERROR (see `0002-44`'s `## Decisions`). The run
  path reusing that decision is the reason to do this at all; diverging needs a stated reason.
- **The built-ins are in the same namespace.** `outrig__*` names are added separately from the
  MCP adapters, and `RESERVED_SERVER` is what keeps them from colliding. Whatever check lands
  should see them too, so the invariant is enforced rather than argued.

## Acceptance

- Two adapters whose names collide are both reachable under distinct names, and the
  diagnostic names both `(server, tool)` pairs.
- A built-in and an MCP tool cannot end up sharing a name silently.
- The tool count the startup banner prints matches the number of names actually registered.

## Dependencies

- `0002-44` (landed) -- it defines the naming rule and the collision policy this reuses.
