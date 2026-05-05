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
