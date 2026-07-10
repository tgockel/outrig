# 0081 -- Dynamic sidecar addition

## Context

Sidecars so far start with the session. This task adds the mid-session surfaces from
[`mcp-sidecars-spec.md`](mcp-sidecars-spec.md), "Dynamic addition": the library API is the
primitive (arbitrary specs), the REPL starts config-declared `start = "manual"` sidecars.
Deliberately no agent-invocable tool -- the agent must not grow its own environment -- and no
`/sidecar stop` in v0.

## Goal

Add sidecars to a live session from the library API (`Outrig::add_sidecar`) and the REPL
(`/sidecar add`), with network policy attached and failures reported to the caller while the
session stays healthy.

## Deliverables

- `Outrig::add_sidecar(SidecarSpec) -> Result<...>`: starts the container (labels, keep-id,
  conditional bootstrap), attaches the network interceptor, connects the spec's servers, and
  extends `tools()`.
- `SidecarSpec` builder in the `LaunchSpec` style: image (config name or raw ref), workspace
  access, mounts, security, servers (name -> command/env).
- `LaunchSpec::with_sidecar(SidecarSpec)` for launch-time declaration by library users.
- REPL: `/sidecar add <name>` (config-declared `start = "manual"` only; unknown or
  already-running names are errors) and `/sidecar list` (status: `running`, `not started`,
  `exited`, plus hosted servers) -- wired into the `repl.rs` command match, `HELP_TEXT`, and a
  callback in `run.rs`.
- Agent toolset refresh: new tools become available on the next turn.
- Dynamic-add failures are reported to the caller only; the session stays healthy.
- Docs updated alongside the code (`usage/mcp.md`, `concepts/mcp-servers.md`,
  `reference/config.md`).

## Acceptance

- A manual sidecar added mid-session serves tools with network policy attached.
- A failed add leaves the session fully usable.
- Unknown names and already-running sidecars are errors in the REPL.

## Dependencies

- **Hard: 0079**. Builds on sidecar start/bootstrap/connect machinery and interceptor attach;
  exec-stdio only, so 0080 is not required.

## See also

- [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md) -- "Dynamic addition" section, decisions 4
  and 11.
- `doc/usage/mcp.md` -- user-facing MCP surface to extend.
