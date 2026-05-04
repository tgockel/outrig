# Library surface for the `outrig` crate

> **Status:** preliminary spec. Carved into numbered tasks in `plan/todo/` when ready.

## Context

`src/lib.rs` today re-exports every module as `pub`:

```rust
pub mod cli;       pub mod config;    pub mod container;  pub mod error;
pub mod hf;        pub mod image;     pub mod init;       pub mod llm;
pub mod mcp;       pub mod process;   pub mod repl;       pub mod repo;
pub mod rig_tool;  pub mod session;
```

That blanket "everything is public" surface was inherited from task 0001 and never
revisited. We want `outrig` to also be usable as a library by another Rust process
that drives its own LLM loop, where the crate's job is narrow:

> Spin up a container described by a Dockerfile + build-args (or an already-built
> image), with one or more MCP servers running inside it. Hand the caller a handle
> they can list and call tools on, and later shut down. No Rig, no agent resolution,
> no REPL -- the caller owns the LLM loop.

This task defines the library surface and the migration shape. The binary
(`bin/outrig.rs`) keeps working unchanged; only the public API contracts as seen by
external users get tightened.

## Goals and non-goals

**In scope:**

- A small, named, deliberately-curated public surface in `outrig::*`.
- A facade type, `Outrig`, that wraps "build (or skip), start, bootstrap, connect
  MCPs, list tools" behind one `launch(spec).await?` call.
- Two ways to obtain a `LaunchSpec`: direct fields, or a project loader pointed at a
  directory containing `.agents/outrig/config.toml`.
- A "no build" path: hand `outrig` an existing image tag and trust that the MCP
  commands declared by the caller already live inside it.
- The CLI binary continues to use today's modules (`cli`, `repl`, `llm`,
  `rig_tool`, `init`, `hf`) without copying or splitting the codebase.

**Out of scope:**

- Exposing Rig, agent resolution, model providers, the REPL harness, or HuggingFace
  metadata as part of the library API. The caller drives its own LLM loop.
- Session bookkeeping (`SessionStore`) as library API. The CLI keeps writing JSON
  records; library callers handle persistence themselves.
- Multi-container orchestration. One `Outrig` handle = one running container.
- Reworking the binary's command flow. `cli/run.rs` keeps composing the same
  primitives; we are not rewriting it on top of `Outrig::launch` in this task.

## Hiding strategy

A new Cargo feature, `internal`, gates the modules that the binary needs but
external library users should not see. The feature is on by default, so
`cargo build` of the binary keeps working with no flag changes; library users opt
out via `default-features = false`.

```toml
[features]
default  = ["internal"]
internal = []
```

`src/lib.rs` becomes:

```rust
// Always public -- the curated library surface.
pub mod error;
pub mod config;        // Config, ContainerConfig, McpServerSpec, Workspace, ApiKeyRef

mod container;         // private; Outrig holds a Container internally
mod mcp;               // private; re-export McpTool, McpToolResult only
mod image;             // private; Outrig::launch drives ensure_image
mod process;           // private
mod repo;              // private
mod session;           // private
mod outrig_;           // new: Outrig, LaunchSpec, WorkspaceSpec, ToolHandle

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
```

(The new module is named `outrig_` internally to avoid the `mod outrig` / crate-name
collision; the *public* re-exports are bare `Outrig`, `LaunchSpec`, etc.)

The `mistralrs` feature stays orthogonal.

## Public surface

### `LaunchSpec`

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
    /// Build the image from a Dockerfile, mount a workspace, run MCPs.
    pub fn build(
        dockerfile: PathBuf,
        context: PathBuf,
        build_args: BTreeMap<String, String>,
        workspace: WorkspaceSpec,
        mcp: BTreeMap<String, McpServerSpec>,
        log_dir: PathBuf,
    ) -> Self;

    /// Skip the build entirely. Trust the image as-is, no workspace mount; MCP
    /// commands must already exist inside the image.
    pub fn from_image(
        image: impl Into<String>,
        mcp: BTreeMap<String, McpServerSpec>,
        log_dir: PathBuf,
    ) -> Self;

    /// Adapter for callers who already hold a parsed `ContainerConfig`. Resolves
    /// relative paths against `repo_root`. Always produces a `Build` source -- for
    /// the no-build path use `from_image` directly.
    pub fn from_container_config(
        cfg: &ContainerConfig,
        workspace: &Workspace,
        repo_root: &Path,
        log_dir: PathBuf,
    ) -> Self;

    pub fn with_workspace(mut self, workspace: WorkspaceSpec) -> Self;
    pub fn without_workspace(mut self) -> Self;
}
```

`McpServerSpec` is re-exported from `config` (already a pure serde enum). `Config`
and `ContainerConfig` stay public for users loading from a `config.toml`, but the
centerpiece API only needs `LaunchSpec`.

### `load_project` -- the directory loader

```rust
pub fn load_project(
    dir: &Path,
    global: Option<&Path>,
) -> Result<(Config, PathBuf)>;
```

Loads `<dir>/.agents/outrig/config.toml`, optionally merging a global config the
same way the CLI does, and returns the parsed `Config` plus the resolved repo root.
A library user pointed at a directory does:

```rust
let (cfg, root) = outrig::load_project(Path::new("./my-repo"), None)?;
let names: Vec<&str> = cfg.containers.keys().map(String::as_str).collect();
// ... show `names` to the end user, get a choice ...
let spec = LaunchSpec::from_container_config(
    &cfg.containers[chosen],
    &cfg.workspace,
    &root,
    log_dir,
);
let outrig = Outrig::launch(&spec).await?;
```

This is a thin wrapper over the existing `Config::load(repo_root, global)` plus
`repo::repo_config_path` / `repo::global_config_path`. No new logic; the helper
exists so the curated surface keeps working when `repo::*` goes private.

### `Outrig` -- the facade

```rust
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

`launch` composes the existing primitives based on `spec.source`:

1. **Image acquisition.**
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

## Internal changes required

- `src/container/mod.rs` -- `Container::start` (and the `podman run` argv builder)
  must be willing to omit `-v` and `-w` when no workspace is supplied. Today both
  flags are unconditional. The `cli/run.rs` caller always passes a workspace, so
  this only adds a code path; it doesn't change existing CLI behavior.
- `src/container/mod.rs::build_exec_argv` returns `process::Cmd` today. Since
  `process` becomes private, either:
    - return `tokio::process::Command` directly (preferred -- adds nothing to the
      public surface and existing callers (`mcp.rs`, `cli/run.rs`) already convert
      to `tokio::process::Command` immediately), or
    - keep `Cmd` and mark `build_exec_argv` itself `pub(crate)`.
- `src/process.rs`, `src/repo.rs`, `src/session.rs` -- module declarations in
  `lib.rs` flip from `pub mod` to `mod`. Items inside become `pub(crate)`. No
  call-site changes; everything that uses them today already lives in the same
  crate.
- `src/mcp.rs` -- `McpClient` becomes `pub(crate)`; `McpTool` and `McpToolResult`
  stay public via re-export from `lib.rs`.
- `src/image.rs` -- `ImageTag`, `ensure_image`, `CacheKey` become `pub(crate)`.

## Files

- `src/lib.rs` -- module visibility flip + feature gating + new `outrig_` module +
  `load_project` free function.
- `Cargo.toml` -- add `[features] internal = []` and `default = ["internal"]`.
- `src/outrig_.rs` (new) -- `Outrig`, `LaunchSpec`, `LaunchSource`, `WorkspaceSpec`,
  `ToolHandle`.
- `src/container/mod.rs` -- visibility flip plus optional-workspace handling in
  `Container::start`.
- `src/mcp.rs` -- visibility flip; keep `McpTool` / `McpToolResult` public.
- `src/image.rs` -- visibility flip.
- `src/process.rs` -- visibility flip; consider returning
  `tokio::process::Command` from `build_exec_argv` instead of leaking `Cmd`.
- `src/repo.rs` -- visibility flip.
- `src/session.rs` -- visibility flip.
- `tests/library_surface.rs` (new, gated `#[cfg(not(feature = "internal"))]` or
  built as a separate test target with `default-features = false`) -- a smoke test
  that drives the public surface only: build a `LaunchSpec::from_image`, launch,
  list tools, call one, shut down.

## Acceptance

- `cargo build --no-default-features` succeeds.
- `cargo doc --no-default-features --no-deps` shows only `error`, `config`,
  `Outrig`, `LaunchSpec`, `WorkspaceSpec`, `ToolHandle`, `McpTool`,
  `McpToolResult`, and `load_project` at the crate root. No `cli`, `hf`, `init`,
  `llm`, `repl`, `rig_tool`, `container`, `mcp`, `image`, `process`, `repo`, or
  `session` modules visible.
- `cargo build` (default features = `internal`) builds the binary unchanged.
- `cargo test` and the existing e2e tests (`--features e2e`) all pass.
- The new smoke test under `tests/` builds against the curated surface only and
  exercises `Outrig::launch` -> `tools()` -> `call_tool()` -> `shutdown()`.

## Dependencies

- None. The seam is already clean: `cli`, `repl`, `llm`, `rig_tool`, `init`, `hf`
  are imported only by `cli/run.rs` and `bin/outrig.rs`; the centerpiece path
  (`container`, `mcp`, `image`, `process`, `repo`) has zero dependency on the
  LLM-side modules.

## Notes

- This is one task in spirit but plausibly two in execution: a first task that
  flips visibility + adds the `internal` feature without behavior change, and a
  second that adds `Outrig` / `LaunchSpec` / `load_project` on top. Whoever
  picks this up should make the call when sequencing into `plan/todo/`.
- The "no build" path (`LaunchSpec::from_image`) is the simpler library entry
  point and a useful foothold for testing -- a fixture image plus a stub MCP
  server lets the smoke test run with no Dockerfile in the loop.
- Optional follow-up, not part of this task: a builder pattern for `LaunchSpec`
  if the constructor list grows past three. Today the three constructors are
  enough.
