# `outrig mcp` -- HTTP / SSE Transport

> **Status:** preliminary spec. Builds on `plan/next/outrig-mcp.md`.

## Context

`plan/next/outrig-mcp.md` ships `outrig mcp` with stdio transport only. Stdio
suits IDEs that spawn the server as a subprocess (Claude Code's default model)
but doesn't help when the user wants:

- A long-running outrig daemon serving multiple IDEs / clients at once.
- Remote clients that aren't on the same machine.
- A persistent container that outlives any single client connection.

rmcp v0.1.5 already supports the server side of streamable HTTP / SSE via the
`transport-sse-server` feature (axum-based). The protocol is the same; only
the transport changes.

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

## Architecture deltas vs. stdio

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

- `plan/next/outrig-mcp.md` -- the v0 stdio version this builds on.
- `~/.cargo/registry/.../rmcp-0.1.5/src/transport/sse_server.rs` -- the axum
  integration point.
