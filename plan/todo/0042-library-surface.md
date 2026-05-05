# 0042 -- Curated `outrig::*` library API

## Goal

Replace the current "every module is `pub`" shape of `src/lib.rs` with a small,
deliberately-curated public surface so external Rust callers can drive a
container with MCP servers without depending on the binary's internals
(`cli`, `repl`, `llm`, `rig_tool`, `init`, `hf`). Centerpiece is a new `Outrig`
facade with a single `launch(spec).await?` entry point. The binary
(`bin/outrig.rs`) keeps working unchanged via a `default = ["internal"]`
feature gate.

The narrowed API answers: "spin up a container described by a Dockerfile +
build-args (or an already-built image), with one or more MCP servers running
inside it. Hand the caller a handle they can list and call tools on, and later
shut down. No Rig, no agent resolution, no REPL -- the caller owns the LLM
loop."

## Deliverables

### Cargo manifest

- `Cargo.toml` -- add:
  ```toml
  [features]
  default  = ["internal"]
  internal = []
  ```
  The `mistralrs` feature stays orthogonal.

### `src/lib.rs` rewrite

```rust
// Always public -- the curated library surface.
pub mod error;
pub mod config;        // Config, ContainerConfig, McpServerSpec, Workspace, ApiKeyRef

mod container;
mod mcp;
mod image;
mod process;
mod repo;
mod session;
mod outrig_;          // new: Outrig, LaunchSpec, WorkspaceSpec, ToolHandle

pub use outrig_::{Outrig, LaunchSpec, WorkspaceSpec, ToolHandle};
pub use mcp::{McpTool, McpToolResult};

pub fn load_project(dir: &Path, global: Option<&Path>) -> Result<(Config, PathBuf)>;

// Gated -- still pub when the feature is enabled, invisible otherwise.
#[cfg(feature = "internal")] pub mod cli;
#[cfg(feature = "internal")] pub mod hf;
#[cfg(feature = "internal")] pub mod init;
#[cfg(feature = "internal")] pub mod llm;
#[cfg(feature = "internal")] pub mod repl;
#[cfg(feature = "internal")] pub mod rig_tool;
#[cfg(feature = "internal")] pub mod tool_name;   // per 0037
```

(The new module is named `outrig_` internally to avoid the `mod outrig` /
crate-name collision; the *public* re-exports are bare `Outrig`,
`LaunchSpec`, etc.)

### New module `src/outrig_.rs`

Public types:

```rust
pub struct LaunchSpec {
    pub(crate) source: LaunchSource,
    pub workspace: Option<WorkspaceSpec>,
    pub mcp: BTreeMap<String, McpServerSpec>,
    pub log_dir: PathBuf,
}

pub(crate) enum LaunchSource {
    Build {
        dockerfile: PathBuf,
        context: PathBuf,
        build_args: BTreeMap<String, String>,
    },
    Image { tag: String },
}

pub struct WorkspaceSpec {
    pub host: PathBuf,
    pub container: PathBuf,
}

impl LaunchSpec {
    pub fn build(
        dockerfile: PathBuf,
        context: PathBuf,
        build_args: BTreeMap<String, String>,
        workspace: WorkspaceSpec,
        mcp: BTreeMap<String, McpServerSpec>,
        log_dir: PathBuf,
    ) -> Self;

    pub fn from_image(
        image: impl Into<String>,
        mcp: BTreeMap<String, McpServerSpec>,
        log_dir: PathBuf,
    ) -> Self;

    pub fn from_container_config(
        cfg: &ContainerConfig,
        workspace: &Workspace,
        repo_root: &Path,
        log_dir: PathBuf,
    ) -> Self;

    pub fn with_workspace(mut self, workspace: WorkspaceSpec) -> Self;
    pub fn without_workspace(mut self) -> Self;
}

pub struct Outrig { /* private: Container, Vec<Arc<McpClient>>, tool index */ }

impl Outrig {
    pub async fn launch(spec: &LaunchSpec) -> Result<Self>;
    pub fn tools(&self) -> &[ToolHandle];
    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<McpToolResult>;
    pub async fn shutdown(self) -> Result<()>;
}

pub struct ToolHandle {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}
```

`launch` composes existing primitives based on `spec.source`:

1. **Image acquisition**:
   - `LaunchSource::Build { .. }` -> `image::ensure_image` (current path: blake3
     cache probe via `buildah images --quiet`, `buildah build` on miss).
   - `LaunchSource::Image { tag }` -> use the tag as-is. No build, no cache lookup.
2. `Container::start` with optional workspace mount based on `spec.workspace`.
3. `container.bootstrap_user()`.
4. For each `(name, spec)` in `spec.mcp`:
   `McpClient::connect_via_podman_exec(&container, spec, name, &spec.log_dir)`,
   collected into `Vec<Arc<McpClient>>`.
5. Call `list_tools()` once per client; build the `ToolHandle` index.

`shutdown` reverses: drop MCP clients via `McpClient::shutdown` (graceful), then
`Container::stop` with the existing 2-second grace window. The same `Drop`
safety-net + panic-hook cleanup paths keep working since `Outrig` owns the
underlying `Container`.

### `load_project` free function

```rust
pub fn load_project(
    dir: &Path,
    global: Option<&Path>,
) -> Result<(Config, PathBuf)>;
```

A thin wrapper over `Config::load(repo_root, global)` plus
`repo::repo_config_path` / `repo::global_config_path`. No new logic; exists so
the curated surface keeps working when `repo::*` goes private.

### Internal changes

- `src/container/mod.rs`:
  - `Container::start` (and the `podman run` argv builder) must accept
    `Option<&WorkspaceSpec>` -- omit `-v` and `-w` when no workspace is
    supplied. Today both flags are unconditional; the `cli/run.rs` caller
    always passes a workspace, so this only adds a code path.
  - `build_exec_argv` returns `process::Cmd` today. Since `process` becomes
    private, either:
    - return `tokio::process::Command` directly (preferred -- adds nothing to
      the public surface and existing callers (`mcp.rs`, `cli/run.rs`) already
      convert to `tokio::process::Command` immediately), or
    - keep `Cmd` and mark `build_exec_argv` itself `pub(crate)`.
- `src/process.rs`, `src/repo.rs`, `src/session.rs` -- module declarations in
  `lib.rs` flip from `pub mod` to `mod`. Items inside become `pub(crate)`. No
  call-site changes; everything that uses them today already lives in the same
  crate.
- `src/mcp.rs` -- `McpClient` becomes `pub(crate)`; `McpTool` and
  `McpToolResult` stay public via re-export from `lib.rs`.
- `src/image.rs` -- `ImageTag`, `ensure_image`, `CacheKey` become `pub(crate)`.
- `src/tool_name.rs` (per 0037) -- decide whether it stays `pub` (for callers
  who want to namespace upstream tool names themselves) or `pub(crate)`. Lean
  toward `pub` so the re-implementation isn't required by external users.

### Tests

- `tests/library_surface.rs` -- new, built as a separate test target with
  `default-features = false`. Drives the public surface only: build a
  `LaunchSpec::from_image`, launch, list tools, call one, shut down.
- The cleanest gating is `#[cfg(not(feature = "internal"))]` plus a
  `[[test]]` entry in `Cargo.toml` that disables default features.

### Files

- `Cargo.toml` -- features + new test target.
- `src/lib.rs` -- module visibility flip + feature gating + new `outrig_`
  module + `load_project` free function.
- `src/outrig_.rs` (new) -- `Outrig`, `LaunchSpec`, `LaunchSource`,
  `WorkspaceSpec`, `ToolHandle`.
- `src/container/mod.rs` -- visibility flip plus optional-workspace handling
  in `Container::start`.
- `src/mcp.rs` -- visibility flip; keep `McpTool` / `McpToolResult` public.
- `src/image.rs` -- visibility flip.
- `src/process.rs` -- visibility flip; consider returning
  `tokio::process::Command` from `build_exec_argv`.
- `src/repo.rs` -- visibility flip.
- `src/session.rs` -- visibility flip.
- `tests/library_surface.rs` (new).

## Acceptance

- `cargo build --no-default-features` succeeds.
- `cargo doc --no-default-features --no-deps` shows only `error`, `config`,
  `Outrig`, `LaunchSpec`, `WorkspaceSpec`, `ToolHandle`, `McpTool`,
  `McpToolResult`, and `load_project` at the crate root. No `cli`, `hf`,
  `init`, `llm`, `repl`, `rig_tool`, `container`, `mcp`, `image`, `process`,
  `repo`, or `session` modules visible.
- `cargo build` (default features = `internal`) builds the binary unchanged.
- `cargo test` and the existing e2e tests (`--features e2e`) all pass.
- The new `tests/library_surface.rs` builds against the curated surface only
  and exercises `Outrig::launch` -> `tools()` -> `call_tool()` -> `shutdown()`.

## Dependencies

None.

## Notes

- **Non-goals (out of scope):**
  - Exposing Rig, agent resolution, model providers, the REPL harness, or
    HuggingFace metadata as part of the library API. The caller drives its own
    LLM loop.
  - Session bookkeeping (`SessionStore`) as library API. The CLI keeps writing
    JSON records; library callers handle persistence themselves.
  - Multi-container orchestration. One `Outrig` handle = one running
    container.
  - Reworking the binary's command flow. `cli/run.rs` keeps composing the
    same primitives; we are not rewriting it on top of `Outrig::launch` here.
- **Phasing.** This is one task in spirit but plausibly two in execution: a
  first pass that flips visibility + adds the `internal` feature without
  behavior change, and a second that adds `Outrig` / `LaunchSpec` /
  `load_project` on top. Whoever picks this up should make the call when
  sequencing.
- **The "no build" path** (`LaunchSpec::from_image`) is the simpler library
  entry point and a useful foothold for testing -- a fixture image plus a stub
  MCP server lets the smoke test run with no Dockerfile in the loop.
- **Builder pattern.** Optional follow-up, not part of this task: a builder
  pattern for `LaunchSpec` if the constructor list grows past three. Today
  the three constructors are enough.
- The seam is already clean: `cli`, `repl`, `llm`, `rig_tool`, `init`, `hf`
  are imported only by `cli/run.rs` and `bin/outrig.rs`; the centerpiece path
  (`container`, `mcp`, `image`, `process`, `repo`) has zero dependency on the
  LLM-side modules. That's why deps = none.
- Lands after the `outrig-mcp` phases (0035-0041) so the new `mcp_proxy` /
  `cli/mcp` modules are part of the public-vs-private decision. Soft
  preference, not a formal dep.
