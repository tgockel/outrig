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

## Decisions

1. **Library `SidecarSpec` image is a raw podman ref only** (user-confirmed). The facade has no
   repo `Config` in scope, so `[images.<name>]` resolution stays a CLI concern
   (`ensure_sidecar_image`); library callers who want a built image run `image::ensure_image`
   themselves. The spec's "config name or raw ref" is satisfied across the two surfaces.
2. **`Outrig::launch` keeps rejecting placement-bearing `LaunchSpec.mcp` entries**
   (user-confirmed); the error now points at `with_sidecar` / `add_sidecar`. Faithful
   config-to-`SidecarSpec` translation in `from_image_config` is deferred to
   `plan/next/from-image-config-sidecar-translation.md`.
3. `SidecarSpec` servers use a dedicated `SidecarServerSpec { command, env }` type instead of
   `McpServerSpec`, making exec-stdio-only a type-level fact rather than a runtime validation.
4. `add_sidecar` returns `Result<Vec<ToolHandle>>` -- the newly added tools (also appended to
   `tools()`) -- so callers don't diff the tool list before/after.
5. Launch-time `with_sidecar` sidecars are abort-only: `SidecarSpec` carries no `on-failure`;
   a library caller holds the `Result` and can relaunch without the sidecar. `warn` remains a
   config/CLI convenience.
6. The CLI's dynamic add composes `ensure_sidecar_image` + `start_one_sidecar` as
   `launch_declared_sidecar(SidecarStartCtx, ...)`; the session-start auto loop keeps its
   inline structure (it interleaves label merges between image-ensure and start) and shares
   the pieces rather than the composition.
7. Manual sidecars need no plan mutation at add time: label merges and duplicate checks
   already run for all named sidecars at session start, before the manual skip, so
   `SessionMcpPlan` stays immutable mid-session.
8. The watcher holds its sidecar list behind `Arc<Mutex<...>>`, read at primary-death time;
   `register_sidecar` covers dynamic adds. It now arms whenever the plan declares any sidecar
   (even all-manual, with an empty started list) so the primary-death token is wired into the
   REPL select before any add.
9. Agent toolset refresh: the rig agent's toolset is frozen at build time, so `/sidecar add`
   appends `McpToolAdapter`s to a shared `Rc<RefCell<Vec<_>>>` and sets a dirty flag;
   `on_prompt` lazily rebuilds the agent (OpenAI: new HTTP client; mistralrs: registry-cached)
   before the next turn. `/tools` recomputes its summary per call.
10. `/sidecar list` shows `unknown` when the podman running-state probe itself errors --
    a rare fourth status beyond the spec'd `running` / `not started` / `exited`, preferred
    over guessing.
11. A failed add unwinds in reverse order (partial clients -> interceptor detach by name ->
    container stop) and reports to the caller only; the store record is updated after commit,
    and a store write failure downgrades to a warning in the success text.
12. `Container::transcript()` went `pub(crate)` -> `pub` so the CLI's mid-session add logs its
    podman commands into the same `container.log` as session start.

## See also

- [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md) -- "Dynamic addition" section, decisions 4
  and 11.
- `doc/usage/mcp.md` -- user-facing MCP surface to extend.
