# `outrig mcp` -- Attach to Existing Container

> **Status:** preliminary spec. Builds on `plan/next/outrig-mcp.md`.

## Context

`plan/next/outrig-mcp.md` ships `outrig mcp` with one container per
invocation. The killer feature it skips: pointing `outrig mcp` at a
container *already* started by an `outrig run` session, so the human's IDE
and the agent share live workspace state.

The model is **shared container, separate MCP children**. We do not try to
multiplex two clients onto one backing MCP server -- that would require a
host-side broker and is much bigger than this feature. Instead: when
attaching, `outrig mcp` spawns its own `podman exec -i` MCP children inside
the running container, alongside the agent's. The container is shared at
the filesystem / process tree layer (workspace, installed tools, env);
MCP-protocol state is per-attacher.

## User surface

```
outrig mcp --attach <session-id-or-container-name> [--container <name>]
```

- `--attach <id>` looks up the session in `<session_root>/<id>/session.json`
  and uses its `container_name`. If the value isn't a known session, treat it
  as a podman container name directly.
- `--container <name>` selects which `[containers.<name>.mcp]` block to
  honor. Required when the attached container's session row is missing or
  has a different config name -- otherwise we don't know which MCP set to
  spawn.

## Architecture deltas vs. fresh-container mode

- The container-start step in `session_setup::setup` becomes optional:
  when `--attach` is set, look up the existing container by name (probe
  via `podman inspect`), validate it's running, and skip image-ensure +
  `Container::start`.
- `Container` gains a constructor `Container::attach(name) -> Self` that
  populates the same fields used downstream (most importantly the exec-argv
  builder) without taking ownership of the lifecycle. Drop semantics for
  attached containers must NOT call `podman stop` -- only owned containers
  do.
- The session entry written by the attacher is its own fresh row. The
  attacher's `session_dir` is distinct from the agent's, so MCP child stderr
  files don't collide.
- `outrig logs <attached-session> <server>` works just like for any session.
- If the host session exits and stops the container while we're attached,
  our MCP children die. Surface as a `CallToolResult` error and exit
  cleanly.

## Edge cases

- **Servers that hold exclusive resources** (a fixed listening port,
  pidfile, global lock file) conflict with their agent-side counterparts.
  Document loudly that MCP servers in this codebase should be
  reentrant-safe; surface conflicts clearly when they happen.
- **Container started by something other than `outrig run`** -- still works
  if the user supplies `--container N` so we know which MCP set to spawn.
- **Attaching twice from two `outrig mcp` invocations.** Each gets its own
  MCP children; same caveat as above on exclusive-resource servers.

## Open sub-decisions

- **Session-id lookup vs. podman-name lookup.** Decide once whether
  `--attach` accepts both (auto-detect) or requires one explicit form.
- **What to do if the attached container disappears mid-session.** Drop
  cleanly? Try to relaunch? Probably drop -- the attacher is a guest, not
  an owner.
- **Visibility into the agent's MCP tool calls.** Stretch goal; out of
  scope here.

## See also

- `plan/next/outrig-mcp.md` -- the v0 fresh-container version.
- `src/container/mod.rs:288-296` -- the existing `Drop for Container`.
- `src/cli/run.rs:155-174` -- the existing teardown order this respects.
