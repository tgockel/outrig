# 0061 -- `outrig mcp` HTTP / SSE transport

## Context

`plan/done/0040-0041` shipped `outrig mcp` with stdio transport only. Stdio
suits IDEs that spawn the server as a subprocess (Claude Code's default model)
but doesn't help when the user wants:

- A long-running outrig daemon serving multiple IDEs / clients at once.
- Remote clients that aren't on the same machine.
- A persistent container that outlives any single client connection.

rmcp v0.1.5 already supports the server side of streamable HTTP / SSE via the
`transport-sse-server` feature (axum-based). The protocol is the same; only
the transport changes.

## Goal

Let `outrig mcp` serve the same session MCP protocol over opt-in streamable HTTP /
SSE while keeping stdio as the default transport.

## User surface

```
outrig mcp [--listen <addr>] [--container <name>] [--session-dir <path>]
```

`--listen <addr>` opts in. Default remains stdio when omitted. Address forms:

- `127.0.0.1:7331` -- TCP loopback.
- `0.0.0.0:7331` -- all interfaces (warn loudly; cross-machine MCP exposes the
  container's tool surface to anything that can reach the port).
- `unix:/tmp/outrig.sock` -- Unix domain socket for tighter ACLs.

Auth: out of scope for the first cut. Local-only by default; callers who want
to expose it broadly should put a reverse proxy with auth in front.

## Deliverables

- The shared bootstrap in `src/cli/session_setup.rs` is unchanged.
- `ProxyServer` is unchanged -- it's transport-agnostic.
- `src/cli/mcp.rs::execute` switches on `--listen`: with no flag, current
  stdio path; with the flag, build an axum router via
  `rmcp::transport::sse_server::SseServerConfig`, bind, and serve.
- Logging discipline relaxes -- stdout is no longer the protocol channel, so
  `println!` is recoverable. Keep the tripwire anyway; consistent UX.
- Multi-client behavior: each connecting client gets its own rmcp service
  instance backed by the same shared `Arc<ProxyServer>`. The backing
  `McpClient`s are still single-connection JSON-RPC pipes -- need to decide
  whether to mux requests over one upstream pipe (likely fine; rmcp client
  handles request IDs) or spawn per-connection clients. Lean toward muxing.

## Acceptance

- `outrig mcp` without `--listen` preserves the current stdio behavior.
- `outrig mcp --listen 127.0.0.1:7331` binds an HTTP / SSE server and serves
  MCP requests through the existing `ProxyServer`.
- `outrig mcp --listen 0.0.0.0:7331` warns loudly about exposing the container
  tool surface beyond loopback.
- Each connecting client gets an independent rmcp service instance backed by the
  shared session proxy.
- Docs describe the listen forms, local-only default, and no-auth v1 stance.

## Open sub-decisions

- **Auth.** Bearer token? mTLS? Unix socket peer-cred only? Unsigned but
  loopback-only? Pick one before shipping; the v0-of-this-feature will likely
  ship with "loopback or unix socket only, no auth" and a doc warning.
- **Multi-client request muxing on a single backing `McpClient`.** rmcp's
  client should handle request-ID multiplexing transparently; verify under
  load before committing.
- **Session lifecycle when no clients are connected.** Reap the container
  after some idle timeout? Keep alive until SIGINT? Probably the latter --
  user invoked it explicitly.

## See also

- `plan/done/0040-outrig-mcp-wire-subcommand.md` -- the v0 stdio version this builds on.
- `plan/done/0041-outrig-mcp-docs.md` -- existing `outrig mcp` documentation.
- `~/.cargo/registry/.../rmcp-0.1.5/src/transport/sse_server.rs` -- the axum
  integration point.

## Dependencies

None hard. The session-MCP `outrig mcp` subcommand has shipped in `plan/done/0040`
and `plan/done/0041`.

## Decisions

- The implementation uses the repo's current `rmcp 1.6` Streamable HTTP server
  API (`StreamableHttpService` / `StreamableHttpServerConfig`) instead of the
  older `rmcp 0.1.5` `sse_server` API named in the original task.
- `--listen` keeps one shared `ProxyServer` and one shared backing
  `Arc<McpClient>` pool. Each HTTP MCP client gets an independent rmcp session
  from `LocalSessionManager`, while upstream tool calls multiplex over the
  existing backing MCP JSON-RPC connections.
- Auth remains out of scope. Loopback TCP keeps rmcp's default host checks;
  non-loopback TCP binds warn and disable those host checks so remote clients
  can connect; Unix sockets rely on filesystem permissions.
- HTTP/SSE mode is daemon-shaped: disconnecting one client closes that MCP
  session only. The process exits on SIGINT, SIGTERM, or attached-container
  shutdown.
