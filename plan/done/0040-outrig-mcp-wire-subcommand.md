# 0040 -- Wire `outrig mcp` subcommand

## Goal

Land the user-visible `outrig mcp` command. Reuses `SessionSetup` (0035) for
container bootstrap and `ProxyServer` (0039) for the MCP-server side, swapping
out `outrig run`'s REPL+agent loop for an rmcp stdio server. Stdio transport
only; HTTP/SSE and attach-mode are deferred follow-ups (still in `plan/next/`).

## Deliverables

- `src/cli/mcp.rs` -- new `McpArgs` struct + `pub async fn execute(...)` mirroring
  `src/cli/run.rs`. Surface:
  ```
  outrig mcp [--container <name>] [--session-dir <path>]
  ```
  | Flag             | Required | Description                                                   |
  |------------------|----------|---------------------------------------------------------------|
  | `--container`    | no       | Pick a `[containers.<name>]`; falls back to `default-container`. |
  | `--session-dir`  | no       | Write the session into an explicit, already-existing directory. |
  Notably **absent**: no `--agent` flag (no agent here). `--container` falls back
  *only* to `default-container`, never to `agent.container`. Failure mode:
  `OutrigError::Configuration("no --container or default-container configured")`.
  Global flags `--config`, `--global-config`, `--session-root` apply unchanged.
- Process flow:
  1. `session_setup::setup` with `agent_flag: None`.
  2. `connect_mcp_clients(&container, &container_cfg, &log_dir)`.
  3. Print one-time banner to **stderr** (sample below).
  4. `let proxy = ProxyServer::build(arcs).await?;`
  5. `let service = rmcp::serve_server(proxy, rmcp::transport::stdio()).await?;`
  6. `tokio::select!` against the running service, `tokio::signal::ctrl_c()`, and
     a `SignalKind::terminate()` stream.
  7. On stdin EOF / SIGINT / SIGTERM: cancel the rmcp service, then the same
     `teardown(...)` shared with `outrig run`.
- `src/bin/outrig.rs` -- add `Cmd::Mcp(McpArgs)` variant + dispatch arm. Keep
  `install_panic_hook()` as for `outrig run`.
- `src/cli/mod.rs` -- `pub mod mcp;`.
- Banner format (stderr only, mirrors `print_banner` in `cli/run.rs`):
  ```
  [outrig] container-config:  coding
  [outrig] image:             outrig:coding-1f3a2b
  [outrig] container started: outrig-coding-2026-05-04-a83f
  [outrig] mcp fs:    initialized (3 tools)
  [outrig] mcp shell: initialized (1 tool)
  [outrig] tools available: fs__list_directory, fs__read_file, fs__write_file, shell__exec
  [outrig] session id: 20260504T141907-a83f
  [outrig] transport: stdio
  [outrig] mcp server ready
  ```
  Uses `eprint!` (not `tracing::info!` -- we don't depend on `OUTRIG_LOG`).
- Stdio discipline tripwires:
  - `#![deny(clippy::print_stdout)]` at the top of `src/cli/mcp.rs` and
    `src/mcp_proxy.rs`.
  - Audit comment confirming binary's tracing-subscriber writes only to stderr
    (already true at `src/bin/outrig.rs:101-102`).
  - Document the load-bearing invariant in 0041's doc page.
- Edge cases handled:
  - Backing MCP crashes mid-session -> `CallToolResult { is_error: Some(true),
    content: [text("outrig: backing server `<name>` call failed: <error>")] }`
    plus a `tracing::warn!` line; log file in `<session_dir>/logs/<name>.stderr`.
  - External client disconnects -> rmcp returns end-of-stream; await completes;
    `teardown` runs.
  - Container dies unexpectedly -> existing `Drop for Container` +
    `install_panic_hook` already cover this.
  - Initialize-time failure (one MCP fails) -> shared
    `connect_mcp_clients` returns `Err` before `serve_server` is called; outrig
    exits before the MCP `initialize` handshake.
  - Zero backing MCPs configured -> error: "outrig mcp with no
    `[containers.<name>.mcp]` entries has nothing to proxy."
- Session row: written via the same `SessionStore`. `agent_name = None` (per
  0036). All other fields identical to `outrig run`.
- `tests/mcp_subcommand_smoke.rs` (`#[cfg(feature = "e2e")]`) -- spawn `outrig
  mcp`, drive it as an MCP client over its stdio (rmcp client side), list tools,
  call one tool, assert clean exit (stdin EOF triggers teardown).

## Acceptance

- `outrig mcp [--container N]` builds, runs, and serves a fully-functional MCP
  server on stdio that an external rmcp client can drive.
- `tools/list` returns the union of every backing server's tools, namespaced
  exactly as `outrig run` namespaces them.
- `tools/call <server>__<name>` dispatches correctly and returns either the
  backing server's content or an `is_error=true` result on failure.
- Stdin EOF, SIGINT, SIGTERM all trigger the same teardown order: rmcp service
  cancel -> `McpClient::shutdown` per server -> `Container::stop` ->
  `SessionStore::finalize`. Exit code: `0` clean, non-zero on startup or cleanup
  error.
- Session row written identically to `outrig run` except `agent_name` is `None`.
- All non-protocol output goes to stderr; an external client receives only valid
  JSON-RPC on its read side. (Verified by the smoke test's strict-JSON decoder.)
- `cargo test --features e2e mcp_subcommand_smoke -- --nocapture` passes.
- All existing tests continue to pass (most importantly `run_smoke`,
  `mcp_handshake`).

## Dependencies

- 0035-outrig-mcp-session-setup
- 0036-outrig-mcp-agent-name-option
- 0039-outrig-mcp-proxy-server

## Notes

- Multi-line REPL input, streaming, per-tool approval, etc. remain deferred --
  this command is a server, not a REPL.
- `prompts/*` and `resources/*` proxying are explicitly out of scope; the trait
  defaults (empty / `method_not_found`) are correct for v0. A future task may
  generalize dispatch into `BackingNamespace<Method>`.

## Decisions

- **`SessionSetupArgs` gains `pub require_agent: bool`** rather than
  reinterpreting `agent_flag: None` as "skip agent entirely." `outrig mcp`
  has no concept of an agent at all -- not an unset flag, not a fallback to
  `default-agent` -- so a separate signal is the cleanest seam. `outrig run`
  passes `true` (today's behavior bit-for-bit); `outrig mcp` passes `false`,
  which also drops `agent_container` from the container fallback chain. The
  `// FIXME(0040)` comment in `session_setup.rs` is gone.

- **`tokio-util` becomes a direct dependency** so `cli/mcp.rs` can hold a
  `CancellationToken` alongside the consumed `RunningService`. The token is
  passed to `rmcp::service::serve_server_with_ct(...)`; on SIGINT/SIGTERM
  we `ct.cancel()` and then `await` the spawned `service.waiting()` so the
  rmcp dispatcher quiesces before `session_setup::teardown` runs. The EOF
  path (peer drops stdin) hits the same `&mut waiter` arm of the
  `tokio::select!` and skips the explicit cancel.

- **`ProxyServer::per_server_counts()` is the new banner-feeder.**
  `iter_public_names` (added by 0039 specifically for the banner) is enough
  for the `tools available:` line, but the per-server `(name, count)` lines
  needed an additional helper. Keeping it on `ProxyServer` avoids a second
  `list_tools` round-trip per server during banner construction.

- **`dispatch_call`'s error path identifies the backing server.** The
  `Err(e)` arm now formats the body as
  `outrig: backing server `<name>` call failed: <error>` and emits a
  matching `tracing::warn!`. The existing `backend_error_surfaces_as_...`
  test already used `.contains` for the inner error, so the prefixed body
  is backwards-compatible; the test gained a second assertion that pins
  the new server-name prefix so the format stays stable.

- **`#![deny(clippy::print_stdout)]` on `cli/mcp.rs` and `mcp_proxy.rs`.**
  Tripwire per the task spec. `outrig run` does not get the same lint
  because its banner is the only stderr-bound `print!`-shaped output and
  it is already isolated; gating that file would force a no-op refactor.

- **Banner uses `eprint!`, not `tracing::info!`.** Spec-mandated and
  load-bearing: the banner must appear regardless of `OUTRIG_LOG`, and
  must not interleave with rmcp's tracing emissions on the same writer
  (both are stderr; the buffered single-write banner stays one block).

- **Signal handling tested via shared code path, not signal injection.**
  All three triggers (stdin EOF, SIGINT, SIGTERM) funnel through one of
  three `tokio::select!` arms that converge on the same `waiter.await`
  -> `teardown` chain. The smoke test exercises the EOF arm explicitly
  (drops the rmcp client, asserts clean exit + container reaped); the
  signal arms exercise the same `ct.cancel(); waiter.await; teardown`
  sequence with no behavioral divergence. Process-level signal injection
  in tests is fragile (race between signal delivery and the select being
  ready); skipping it favors signal-via-code-review over a flaky test.
