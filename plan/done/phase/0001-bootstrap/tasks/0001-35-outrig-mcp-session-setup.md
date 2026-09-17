# 0035 -- Refactor: extract `SessionSetup` from `cli/run.rs`

## Goal

Pure refactor of `src/cli/run.rs` that lifts the shared bootstrap (config-load,
container resolution, image-ensure, container-start, runtime-user bootstrap, session
row, log-dir, MCP child spawn loop) into a new `src/cli/session_setup.rs` so the
upcoming `outrig mcp` subcommand can reuse it. `outrig run` continues to behave
identically.

## Deliverables

- New `src/cli/session_setup.rs` exposing:
  - `pub struct SessionSetup` -- holds `cfg: Config`, `container_cfg_name: String`,
    `container_cfg: ContainerConfig`, `image_tag: ImageTag`, started+bootstrapped
    `container: Container`, `sid: SessionId`, `session: Session`,
    `session_dir: PathBuf`, `log_dir: PathBuf`, `store: SessionStore`.
  - `pub struct SessionSetupArgs<'a>` with `repo_cfg_path`, `global_cfg_path`,
    `session_root_flag`, `container_flag`, `agent_flag` (`Option<&str>`,
    `None` for `outrig mcp` later), `explicit_session_dir`.
  - `pub async fn setup(args: SessionSetupArgs<'_>) -> Result<SessionSetup>` --
    everything from "load configs" through "container started + bootstrapped + session
    row + log dir created", but stops before MCP children connect.
  - `pub async fn connect_mcp_clients(container: &Container,
    container_cfg: &ContainerConfig, log_dir: &Path) -> Result<Vec<Arc<McpClient>>>`
    -- spawns one `McpClient` per declared backing MCP, in config-map iteration order.
  - `pub async fn teardown(mcp_arcs: Vec<Arc<McpClient>>, container: Container,
    store: &SessionStore, sid: &SessionId, final_exit: i32)` -- mirror of run.rs's
    cleanup tail (graceful `McpClient::shutdown` per server -> `Container::stop` ->
    `SessionStore::finalize`).
- `src/cli/run.rs::execute` rewritten to delegate to `setup` + `connect_mcp_clients`,
  resolve agent + build adapters via `McpToolAdapter::from_client_tools`, build agent,
  banner, `Repl::run`, and finally `teardown`.
- `src/cli/mod.rs` -- add `pub mod session_setup;`.

## Acceptance

- `cargo build` clean.
- `cargo test` and `cargo test --features e2e` (where prereqs are available) pass with
  no behavioral change for `outrig run`.
- `clippy` clean; `fmt` clean.
- The same banner lines, the same session-row fields, the same teardown order as before
  the refactor.

## Dependencies

None.

## Notes

- The `agent_flag: Option<&str>` parameter is the seam that makes this reusable: `None`
  signals "no agent resolution -- container falls back only to `default-container`,
  not to `agent.container`." `outrig run` always passes `Some(...)`; the failure-mode
  string for `outrig mcp` is documented in 0040.
- This task touches the public API of `src/cli` only as far as adding the new module;
  external surface (binary CLI, config TOML, on-disk session format) is unchanged.
- Cross-references to the master spec sections "Module layout" and the `SessionSetup`
  Rust sketch (in the original `outrig-mcp.md`, now split across 0035-0041).

## Decisions

1. **Agent presence error stays in `setup`.** When both `agent_flag` and
   `cfg.default_agent` are `None`, `setup` errors with `"no --agent and no
   default-agent configured"` -- exactly today's run.rs message, and emitted
   *before* any container work, matching today's error ordering bit-for-bit.
   The spec's "`None` for `outrig mcp` later" gloss is deferred to 0040: the
   future caller will need either a different code path or a relaxed check
   here. A `// FIXME(0040)` comment marks the spot.
2. **`SessionSetup` does not carry the resolved agent.** Matches the
   deliverable spec literally. `run.rs` re-resolves via `llm::resolve_agent`
   for the `build_agent` + banner step. The duplicate call is cheap (config
   table lookups, no I/O) and keeps `llm::ResolvedAgent` out of the
   `cli::session_setup` public type surface.
3. **`run.rs` reads the agent name from `setup.session.agent_name`** rather
   than re-running the `args.agent.or(cfg.default_agent)` fallback. This
   removes a duplicate fallback chain and an `.expect("setup validated...")`
   that would have coupled `run.rs` to setup's internal contract.
4. **`STOP_GRACE` moves to `session_setup`** as `pub(crate) const`.
   `teardown` is the only caller.
5. **`connect_mcp_clients` returns `Vec<Arc<McpClient>>` only.** Adapter
   construction (`McpToolAdapter::from_client_tools`) stays in `run.rs`
   because the REPL is the only consumer of adapters; `outrig mcp` will
   expose the same clients differently.
6. **`run.rs::execute` populates `mcp_arcs` incrementally** through a
   `&mut Vec<Arc<McpClient>>` parameter on the new `run_inner` helper.
   This preserves today's invariant: any client that successfully connected
   gets explicitly shut down by `teardown`, even if a later step
   (adapter build, agent build) fails partway through.
7. **Teardown's tracing target moves from `outrig::cli::run` to
   `outrig::cli::session_setup`** (matches the new module path; the smoke
   test does not filter on target).
8. **Adapters and the agent are explicitly `drop()`ped before teardown.**
   They each hold `Arc<McpClient>` clones; without the explicit drops,
   `Arc::try_unwrap` in `teardown` would fail and the explicit `shutdown`
   would be skipped in favor of `Drop`.
9. **`run_repl` slims to two args** (`&RigAgent`, `tools_summary: String`);
   the banner moves into the new `run_inner` helper. The new helper
   accumulates 10 args and keeps `#[allow(clippy::too_many_arguments)]`,
   matching the original `run_repl`'s allow.
