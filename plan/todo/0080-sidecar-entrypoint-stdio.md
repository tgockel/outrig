# 0080 -- Sidecar entrypoint-stdio transport

## Context

With sidecar core landed (0079), the inline `{ image = "..." }` form with no `command` gains
meaning: the image's ENTRYPOINT *is* the MCP server, enabling off-the-shelf MCP images (the
Docker MCP catalog pattern) with zero repo-side command knowledge. See
[`mcp-sidecars-spec.md`](mcp-sidecars-spec.md), "Transports" (entrypoint-stdio). The hard part
is attaching network interception before the entrypoint can emit a packet.

## Goal

Support entrypoint-stdio MCP servers: `{ image = "<ref>", env = {...} }` with no command runs
the image's ENTRYPOINT as the server over piped stdio, with container lifetime equal to server
lifetime and network policy applied from the first packet.

## Deliverables

- The no-command inline form: `podman run -i` with piped stdio; `env` becomes `--env` flags.
- Interception attached before the entrypoint runs. Candidate mechanism to validate:
  `podman create` + `podman init` to materialize the container process and netns without
  executing the entrypoint, attach the interceptor to the created PID, then
  `podman start --attach --interactive` for the stdio pipes.
- Lifetime-equals-server semantics: server exit surfaces exactly like a mid-session sidecar
  death (logged, tools error, session survives).
- Validation already specified in 0079 holds: an entry with `image` and no `command` may carry
  `env` but nothing else.
- Docs updated alongside the code (`concepts/mcp-servers.md`, `usage/mcp.md`,
  `reference/config.md`).

## Acceptance

- An off-the-shelf MCP image runs as a server with session network policy applied from its
  first packet.
- Server exit surfaces as tool errors without killing the session.

## Dependencies

- **Hard: 0079**. Extends the sidecar container machinery with a second transport.

## See also

- [`mcp-sidecars-spec.md`](mcp-sidecars-spec.md) -- "Transports" section.
- `doc/concepts/mcp-servers.md` -- MCP transport narrative to extend.
