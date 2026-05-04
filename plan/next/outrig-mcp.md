# `outrig mcp` Subcommand

> **Status:** preliminary spec. Carved into numbered tasks in `plan/todo/` when
> ready.

## Context

`outrig run` builds an interactive REPL on top of a containerized stack of MCP
servers: it loads config, ensures an image, starts a podman container,
bootstraps the runtime user, opens an `McpClient` (rmcp `RoleClient`) to each
declared server, registers their tools with a Rig agent, and drives a stdin/
stdout REPL. The container and MCP children are torn down on exit.

`outrig mcp` reuses that pipeline through "MCP children connected" and replaces
the Rig agent + REPL with a stdio MCP server (rmcp `RoleServer`). External
clients -- Claude Code, Cursor, Zed, anything that consumes MCP -- spawn
`outrig mcp` instead of speaking directly to a containerized server. From their
point of view, outrig *is* a single MCP server whose tools are the union of
every backing server's tools, namespaced as `<server>__<tool>` exactly like
the agent sees them today.

The pitch: outside-the-container MCP clients want sandboxed tools without each
one having to know how to drive podman. outrig already knows.

## Goals and non-goals

**In scope (v0):**

- New `outrig mcp` subcommand parallel to `outrig run`.
- Stdio transport only.
- Fresh container per invocation, full v0 lifecycle.
- One aggregated MCP server fronting every backing MCP, namespaced
  `<server>__<tool>`.
- Full `Session` row written, same as `outrig run`.
- All tracing/diagnostic output to stderr; stdout is reserved for JSON-RPC.

**Out of scope (deferred to follow-up `plan/next/` entries):**

- HTTP/SSE transport -> `plan/next/outrig-mcp-http-sse.md`.
- Attach-to-existing-container mode -> `plan/next/outrig-mcp-attach.md`.
- `prompts/*` and `resources/*` proxying. v0 proxies `tools/*` only.
- Tool-call audit log (cross-cutting; separate feature).
- Auto-restart of crashed backing MCPs (already deferred for `outrig run` in
  `doc/concepts/mcp-servers.md`; same stance here).

## User surface

```
outrig mcp [--container <name>] [--session-dir <path>]
```

| Flag             | Type   | Required | Default | Description                                                                                  |
|------------------|--------|----------|---------|----------------------------------------------------------------------------------------------|
| `--container`    | string | no       | --      | Pick a `[containers.<name>]` block; falls back to top-level `default-container`.             |
| `--session-dir`  | path   | no       | --      | Write the session into an explicit, already-existing directory (same as `outrig run`).        |

Notably **absent** from `outrig mcp` (vs. `outrig run`):

- No `--agent`. There is no agent -- this command is a server, not a client of
  an LLM. The agent-resolution branch in `src/cli/run.rs:62-69` is skipped.
- `--container` falls back only to `default-container`, not to
  `agent.container`. Failure mode: `OutrigError::Configuration("no --container
  or default-container configured")`.

The global flags `--config`, `--global-config`, and `--session-root` apply
unchanged.

### Process model

Once the container is up and every backing MCP is connected, `outrig mcp`:

1. Prints a one-time banner to **stderr** (sample below).
2. Calls `rmcp::serve_server(handler, rmcp::transport::stdio())` and awaits the
   resulting `RunningService` to completion.
3. On stdin EOF, SIGINT, or SIGTERM, gracefully tears down: cancel the rmcp
   service, run the same `McpClient::shutdown()` loop `outrig run` uses, then
   `Container::stop`, then `SessionStore::finalize`.

Exit code mirrors `outrig run`: `0` clean, non-zero on startup or cleanup
error.

## Architecture

### Module layout

```
src/cli/mcp.rs                # NEW. McpArgs + execute(); mirrors src/cli/run.rs.
src/cli/session_setup.rs      # NEW. Shared bootstrap: config-load, container
                              # resolution, image-ensure, container-start,
                              # bootstrap, session row, log-dir, MCP child
                              # spawn loop. Both run.rs and mcp.rs consume it.
src/mcp_proxy.rs              # NEW. ProxyServer (impl rmcp::ServerHandler).
                              # Holds the McpClient pool, dispatches list_tools
                              # and call_tool. Strips/adds the `<server>__`
                              # prefix.
src/tool_name.rs              # NEW. The `sanitize(server, tool)` helper and
                              # constants moved out of src/rig_tool.rs (single
                              # source of truth, used by both rig_tool and
                              # mcp_proxy).
src/cli/mod.rs                # add `pub mod mcp;`
src/bin/outrig.rs             # add Cmd::Mcp variant + dispatch arm.
```

Why factor out `session_setup` rather than stuff it into `run.rs`: the future
attach-mode follow-up is the third caller and it's already on the radar.
Pulling shared orchestration out is a one-time refactor that pays for itself
the moment a third caller appears.

The shared module's surface, roughly:

```rust
// src/cli/session_setup.rs

pub struct SessionSetup {
    pub cfg: Config,
    pub container_cfg_name: String,
    pub container_cfg: ContainerConfig,
    pub image_tag: ImageTag,
    pub container: Container,         // started + bootstrapped
    pub sid: SessionId,
    pub session: Session,
    pub session_dir: PathBuf,
    pub log_dir: PathBuf,
    pub store: SessionStore,
}

pub struct SessionSetupArgs<'a> {
    pub repo_cfg_path: &'a Path,
    pub global_cfg_path: &'a Path,
    pub session_root_flag: Option<&'a Path>,
    pub container_flag: Option<&'a str>,
    pub agent_flag: Option<&'a str>,         // None for outrig mcp.
    pub explicit_session_dir: Option<&'a Path>,
}

pub async fn setup(args: SessionSetupArgs<'_>) -> Result<SessionSetup>;

/// Spawns one McpClient per declared backing MCP. Returns the Arcs in the
/// order they appear in the config map.
pub async fn connect_mcp_clients(
    container: &Container,
    container_cfg: &ContainerConfig,
    log_dir: &Path,
) -> Result<Vec<Arc<McpClient>>>;

/// Mirror of run.rs's existing cleanup tail.
pub async fn teardown(
    mcp_arcs: Vec<Arc<McpClient>>,
    container: Container,
    store: &SessionStore,
    sid: &SessionId,
    final_exit: i32,
);
```

`outrig run`'s `execute` shrinks to: `setup` -> resolve agent -> build
adapters via `McpToolAdapter::from_client_tools` -> build agent -> banner ->
`Repl::run` -> `teardown`.

`outrig mcp`'s `execute` becomes: `setup` -> build `ProxyServer` from the same
`Arc<McpClient>`s -> stderr banner -> `serve_server(handler, stdio()).await`
-> wait on the running service -> `teardown`.

### `ProxyServer` (the rmcp `ServerHandler`)

Sketch:

```rust
// src/mcp_proxy.rs

pub struct ProxyServer { inner: Arc<ProxyInner> }

struct ProxyInner {
    clients: Vec<Arc<McpClient>>,
    tools:   Vec<ToolEntry>,
    by_public_name: HashMap<String, usize>,
    server_info: ServerInfo,
}

struct ToolEntry {
    public_name: String,                             // e.g. fs__read_file
    backend_tool: String,                            // upstream name
    description: String,
    input_schema: Arc<serde_json::Map<String, Value>>,
    client_idx: usize,
}

impl ProxyServer {
    pub async fn build(clients: Vec<Arc<McpClient>>) -> Result<Self> { ... }
    pub fn iter_public_names(&self) -> impl Iterator<Item = &str> { ... }
}

impl ServerHandler for ProxyServer {
    fn get_info(&self) -> ServerInfo { self.inner.server_info.clone() }

    async fn list_tools(&self, _: PaginatedRequestParam, _: RequestContext<RoleServer>)
        -> Result<ListToolsResult, rmcp::Error> { ... }

    async fn call_tool(&self, req: CallToolRequestParam, _: RequestContext<RoleServer>)
        -> Result<CallToolResult, rmcp::Error> {
        // Lookup public_name -> entry; map None -> CallToolResult { is_error: true }.
        // Forward to clients[entry.client_idx].call_tool(entry.backend_tool, args).
        // Backing-client Err -> CallToolResult { is_error: true } with text body.
    }
}
```

Why the dynamic-handler approach (overriding `list_tools` / `call_tool`) and
not the `#[tool]` macros: the `#[tool]` macros register tools at compile time
from typed Rust functions. Our tool set is unknown until runtime. The
trait-default-override path is what's left, and it's exactly what we want. We
do **not** override `list_resources`, `list_prompts`, etc. -- the trait
defaults already return empty / `method_not_found`, which is correct for v0.

### Tool name aggregation

Reuse the `<server>__<tool>` sanitizer from `src/rig_tool.rs:123-146`. Move it
and its constants (`MAX_NAME_LEN`, `HASH_HEX_LEN`, `SUFFIX_LEN`) into a new
small module `src/tool_name.rs`. `src/rig_tool.rs` and `src/mcp_proxy.rs` both
import from there. No re-export shims; both call sites switch in the same
commit.

Tool descriptions and `input_schema` pass through unchanged. The
`by_public_name` HashMap maps the public name back to a
`(client_idx, backend_tool)` pair, so prefix stripping is implicit.

A startup-time assertion catches public-name collisions across servers; if
`sanitize`'s blake3 suffix scheme ever regresses, we want a loud failure.

### Cargo features

Add `"server"` and `"transport-io"` to the `rmcp` feature list in
`Cargo.toml`. Verified against rmcp 0.1.5: `serve_server` is gated behind
`server`; `rmcp::transport::stdio()` is gated behind `transport-io`. No other
rmcp feature changes.

```toml
[dependencies.rmcp]
version = "0.1"
default-features = false
features = [
    "base64",
    "client",
    "macros",
    "server",            # NEW: ServerHandler trait, serve_server
    "transport-child-process",
    "transport-io",      # NEW: rmcp::transport::stdio()
]
```

## The stdio gotcha (logging discipline)

`outrig mcp` MUST NOT write a single non-JSON-RPC byte to stdout. The rmcp
codec is line-delimited JSON; one stray `println!` corrupts framing and the
external client's decoder errors out.

Audit (codified here so the task author preserves it):

- `src/bin/outrig.rs:101-102` -- `tracing_subscriber::fmt().with_writer(std::io::stderr)`.
  Already correct.
- `src/cli/run.rs:302` -- `eprint!`. Already stderr.
- `src/init/**`, `src/config/init.rs`, `src/llm.rs:375,415` -- not on the
  `outrig mcp` code path (no agent, no init prompts).
- `src/repl.rs` -- not invoked.

New disciplines:

1. **No `println!`/`print!` on the `outrig mcp` code path.** Add
   `#![deny(clippy::print_stdout)]` at the top of `src/cli/mcp.rs` and
   `src/mcp_proxy.rs` as a tripwire.
2. **Tracing target check.** Confirm the binary's tracing-subscriber writes
   only to stderr (it does); call this out as a load-bearing invariant in the
   doc page.
3. **Backing-MCP stderr is fine.** `McpClient` redirects each child's stderr
   to `<log_dir>/<name>.stderr` (`src/mcp.rs:73-79`). Child stdout is consumed
   by us; nothing leaks to outrig's stdout.
4. **Banner uses `eprint!`.** Same way `print_banner` does
   (`src/cli/run.rs:302`). Not `tracing::info!` -- that depends on the user's
   `OUTRIG_LOG` filter.

## Error / lifecycle edge cases

- **Backing MCP crashes mid-session.** Surface as
  `CallToolResult { is_error: Some(true), content: [text("outrig: backing
  server `<name>` call failed: <error>")] }`. External client sees a
  structured tool-call error; user sees a `tracing::warn!` line and the
  backing server's own stderr in `<session_dir>/logs/<name>.stderr`.
  Subsequent `tools/list` still returns the dead server's tools (v0; matches
  `outrig run`).

- **External client disconnects.** rmcp's stdio transport returns end-of-stream
  on stdin EOF; the `RunningService` future completes. Our `await` returns,
  `teardown` runs. No special detection.

- **SIGINT / SIGTERM.** Wrap the service await in a `tokio::select!` against
  `tokio::signal::ctrl_c()` and a `SignalKind::terminate()` stream. On signal,
  cancel the rmcp service, then fall into the same teardown path. `tokio` is
  already built with `signal` (`Cargo.toml:56`).

- **Container dies unexpectedly.** Existing `Drop for Container` plus
  `install_panic_hook()` in `src/container/mod.rs:338-351` handle leaked
  containers. `outrig mcp` calls `install_panic_hook()` the same way the
  binary does today (`src/bin/outrig.rs:104`).

- **Initialize-time failure (one MCP fails to come up).** Same as
  `outrig run`: fail fast, exit before serving anything. The shared
  `connect_mcp_clients` helper returns `Err` on the first failed
  `McpClient::connect_via_podman_exec`; we never reach `serve_server`. The
  external client sees outrig die before the MCP `initialize` handshake -- the
  right shape, since partial sandboxes are worse than visible failures.

- **Zero backing MCPs configured.** Error rather than serve an empty tool
  list: "outrig mcp with no `[containers.<name>.mcp]` entries has nothing to
  proxy." Catches user typos faster.

## Sessions integration

`Session::agent_name` is currently `String` (`src/session.rs:83`). Make it
`Option<String>`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub agent_name: Option<String>,
```

Reasoning: a `None` value is meaningfully different from any sentinel; it
tells `outrig ls` and `outrig logs` "this session was not driven by an agent"
without forcing them to special-case a magic string.

Sites to update:

- `src/cli/run.rs:126` -- `agent_name: Some(resolved.agent_name.clone())`.
- `src/cli/mcp.rs` (new) -- `agent_name: None`.
- `tests/common/mod.rs:94` -- `agent_name: Some("default".into())`.
- Display sites (`src/cli/ls.rs`, anywhere else formatting `agent_name`) --
  `session.agent_name.as_deref().unwrap_or("-")` or similar.

Grep gate: after the change, `grep -rn "agent_name" src/ tests/` should return
only Option-aware uses.

## Banner / startup output

Mirrors `src/cli/run.rs::print_banner`, going strictly to stderr. Three
differences:

1. No agent / model / provider line.
2. Adds `transport: stdio` so the user knows which mode this is in
   (forward-compat with HTTP/SSE).
3. Adds a `mcp server ready` line at the end and *flushes* stderr.

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

## Phased delivery

Each phase becomes one numbered `plan/todo/NNNN-*` task. Order matters --
later phases depend on the refactors landing first.

### Phase 1 -- refactor: extract `SessionSetup`

Pure refactor of `src/cli/run.rs`. New `src/cli/session_setup.rs` exposes
`setup`, `connect_mcp_clients`, `teardown`. `outrig run` works identically.
Tests pass before merge.

### Phase 2 -- refactor: `Session::agent_name` -> `Option<String>`

Touches `src/session.rs`, the run.rs write site, all read sites,
`tests/common/mod.rs`. Grep gate above; tests pass before merge.

### Phase 3 -- refactor: factor `tool_name::sanitize` out of `rig_tool.rs`

Move sanitize + constants to `src/tool_name.rs`. Both call sites update in the
same commit; no re-export shim.

### Phase 4 -- add rmcp `server` + `transport-io` features

One-line `Cargo.toml` change. Verify `cargo build` and `cargo test` are clean.

### Phase 5 -- land `ProxyServer`

`src/mcp_proxy.rs` plus a unit test (`tests/mcp_proxy_dispatch.rs`) using a
small `BackingClient` trait so the proxy can be exercised against in-process
fakes. No CLI surface yet.

### Phase 6 -- wire `outrig mcp`

`src/cli/mcp.rs`, `Cmd::Mcp` variant, dispatch, banner, signal handling.
End-to-end smoke test (`tests/mcp_subcommand_smoke.rs`, `--features e2e`):
spawn `outrig mcp`, drive it as an MCP client over its stdio, list and call
tools, assert clean exit.

### Phase 7 -- docs

`doc/usage/mcp.md`, `doc/SUMMARY.md` entry, `doc/reference/cli.md` subsection,
header note in `doc/concepts/mcp-servers.md` pointing at the new page.

## Acceptance

- `outrig mcp [--container N]` builds, runs, serves a fully-functional MCP
  server on stdio that an external client can drive.
- `tools/list` returns the union of every backing server's tools, namespaced
  exactly like `outrig run` namespaces them today.
- `tools/call <server>__<name>` dispatches correctly and returns either the
  backing server's content or an `is_error=true` result on failure.
- Stdin EOF, SIGINT, SIGTERM all trigger the same teardown order: rmcp
  service cancel -> `McpClient::shutdown` per server -> `Container::stop` ->
  `SessionStore::finalize`.
- Session row written identically to `outrig run` except `agent_name` is
  `None`.
- All non-protocol output goes to stderr; an external client receives only
  valid JSON-RPC on its read side.
- `cargo test --features e2e mcp_subcommand_smoke -- --nocapture` passes.
- All existing tests continue to pass (most importantly `run_smoke`,
  `mcp_handshake`).
- `doc/usage/mcp.md` and the `doc/SUMMARY.md` entry are present.

## Open sub-decisions

- **`ServerInfo` defaults.** `name = "outrig"`,
  `version = env!("CARGO_PKG_VERSION")`, and a one-line `instructions` string
  mentioning the `<server>__<tool>` namespace prefix scheme so the LLM sees it.
  Final wording deferred.
- **`tools/list` pagination.** v0 returns one page (`next_cursor: None`).
  Pagination becomes worth it only with hundreds of tools; not in v0.
- **Backing-server stderr surfacing to the external client.** Today the user
  reads `<session_dir>/logs/<name>.stderr` via `outrig logs`. Exposing it as
  an MCP `resource` is a v1 idea; out of scope here.
- **`prompts/*` and `resources/*` proxying.** Same shape as tool aggregation.
  v0 returns the empty defaults the trait gives us. The task that adds
  proxying will likely lift dispatch into a generic
  `BackingNamespace<Method>` over `tools` / `prompts` / `resources`. Out of
  scope for v0.

## Doc updates required when this ships

- `doc/usage/mcp.md` -- new page (the bulk).
- `doc/SUMMARY.md` -- new entry under **Usage**, between `outrig run` and
  `outrig build`.
- `doc/reference/cli.md` -- new `mcp` subsection mirroring the existing `run`
  subsection.
- `doc/concepts/mcp-servers.md` -- one paragraph at the top noting that the
  same `[containers.<name>.mcp]` table is consumed by both `outrig run` and
  `outrig mcp`, with a forward-link to `doc/usage/mcp.md`.
- `doc/usage/sessions.md` -- mention that `outrig mcp` sessions have no
  `agent_name` and display as `-`.

## See also

- `doc/concepts/mcp-servers.md` -- the model `outrig mcp` republishes.
- `src/cli/run.rs` -- the orchestration this command parallels.
- `src/rig_tool.rs:123-146` -- the `<server>__<tool>` naming source of truth.
- `src/mcp.rs` -- the `McpClient` primitives the proxy reuses verbatim.
- `plan/next/outrig-mcp-http-sse.md` -- HTTP/SSE follow-up.
- `plan/next/outrig-mcp-attach.md` -- attach-to-existing-container follow-up.
