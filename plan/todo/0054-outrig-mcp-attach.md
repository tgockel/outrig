# 0054 -- `outrig mcp --attach` to an existing container

## Context

`plan/done/0035-0041` shipped `outrig mcp` with one container per
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

## Goal

Let `outrig mcp` attach to a container started by an `outrig run` session,
sharing the workspace, installed tools, and environment while running its
own MCP children alongside the agent's.

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

## Deliverables

- New `Container::attach(name) -> Self` constructor that populates the
  fields downstream code reads (exec-argv builder primarily) without
  taking lifecycle ownership.
- `Drop for Container` distinguishes owned from attached: only owned
  containers `podman stop` on drop.
- `session_setup::setup` gains an "attach" branch that skips
  `image::ensure_image` and `Container::start`, probes the container
  via `podman inspect`, and validates it's running.
- `--attach <session-id-or-name>` flag on the `outrig mcp` subcommand,
  with optional `--container <name>` for cases where the session row
  doesn't carry a config name.
- Session-row write path used by the attacher writes a fresh row in its
  own `session_dir` -- distinct from the host session's so MCP child
  stderr files don't collide.
- Doc update: `doc/concepts/mcp-servers.md` (attach mode + the
  reentrant-safe MCP server expectation), and the existing
  `outrig mcp` docs (`doc/usage/mcp.md` or wherever 0041 placed them)
  pick up the `--attach` flag.
- Tests: integration coverage for attach-by-session-id, attach-by-name,
  attach-when-host-stops-container (clean exit), and the
  Drop-doesn't-stop semantic.

## Acceptance

- `outrig mcp --attach <session-id>` resolves the session row, reuses
  its container, and spawns the resolved `[containers.<name>.mcp]` set
  inside it. The agent's existing MCP children keep running.
- `outrig mcp --attach <podman-name> --container <cfg>` works against a
  container that wasn't started by `outrig run`.
- When the attacher exits cleanly, the container is unaffected -- the
  host `outrig run` session keeps running.
- When the host session stops the container while an attacher is live,
  the attacher's MCP children die and the attacher exits with a clear
  error rather than hanging.
- `outrig logs <attached-session> <server>` works against the
  attacher's session dir just like any other session.

## Dependencies

None hard. The session-MCP `outrig mcp` subcommand has shipped
(`plan/done/0035-0041`); this task builds on it.

## See also

- `plan/done/0035-0041` -- the v0 fresh-container `outrig mcp` this
  builds on.
- `src/container/mod.rs:288-296` -- the existing `Drop for Container`.
- `src/cli/run.rs:155-174` -- the existing teardown order this respects.
