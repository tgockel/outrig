# 0053 -- Embedded `container.toml` -- image-side MCP config

## Context

Today, an image and the list of MCP servers running inside it are split across
two files: the `Dockerfile` (in `.agents/outrig/containers/<name>/`) and the
repo's `config.toml`, with `[containers.<name>.mcp]` listing the servers and
their commands. To share a container-config across repos, the user has to ship
both halves and keep them in sync -- the image needs the binaries, the config
needs the matching `mcp` block.

Bundling the `[mcp]` block into the image itself, at a known path, lets the
image be the source of truth for "what tools live here". `config.toml` is
reduced to picking a container and (optionally) overriding individual entries.
The same well-known file is a natural home for other image-scoped metadata
later (workspace hints, default env overlays, label strings) -- hence the
generic name `container.toml` rather than a single-purpose `mcp.toml`.

Existing surfaces this builds on:

- `ContainerConfig.mcp: BTreeMap<String, McpServerSpec>` at
  `src/config/mod.rs:168` -- the shape the merged result has to match.
- `connect_via_podman_exec` in `src/mcp.rs` -- the function that consumes each
  spec to spawn an MCP child.
- The MCP startup loop at `src/cli/run.rs:201-208` -- the place where the
  merge happens.
- `podman_exec_root` helper in `src/container/mod.rs:305` -- already wraps
  `podman exec --user=0:0 <name>`, which is what we need to read the file.
- `doc/concepts/mcp-servers.md` -- the user-facing doc that gains a new
  subsection.
- `doc/reference/config.md` -- cross-links to the new doc.

## Goal

At session start, after `Container::start` and `bootstrap_user` have run,
outrig reads `/etc/outrig/container.toml` from inside the container, parses
its `[mcp]` table, and merges it with `[containers.<name>.mcp]` from
`config.toml`. The merged map drives the existing MCP startup loop. A missing
file is silent (falls back to `config.toml` only); a malformed file is a hard
error.

## Known path inside the image

`/etc/outrig/container.toml`. Chosen because:

- `/etc/<vendor>/...` is a long-standing Linux convention for service config.
- Owned by image-build, not the workspace mount; can't be accidentally
  clobbered by a `--mount` over `/workspace`.
- Single shared path -- no need to search multiple locations.
- Generic filename leaves room for future sections without inventing a second
  well-known path.

## File format

```toml
# /etc/outrig/container.toml
[mcp]
fs    = { command = ["mcp-server-filesystem", "/workspace"] }
shell = ["bash", "-lc", "exec mcp-server-shell"]
build = { command = ["cargo-mcp"], env = { CARGO_HOME = "/workspace/.cargo" } }
```

The `[mcp]` schema is **identical** to the existing
`[containers.<name>.mcp]` block: each entry is either the **short form** (a
bare command array) or the **full form** (a table with `command` and optional
`env`). A user migrating an existing config can copy the inner table into the
image's `container.toml` unchanged.

Unknown top-level tables are tolerated and ignored. The parser uses a typed
struct with `serde(default)` for known sections and ignores the rest, so an
image that includes forward-looking sections (e.g. `[workspace]`,
`[env]`, `[labels]`) won't break older outrig binaries.

## Architecture

### Extraction

`podman exec -i --user=0:0 <container> cat /etc/outrig/container.toml`,
issued once per session, immediately after `bootstrap_user` returns. Reuses
the existing `podman_exec_root` helper.

- `cat` exiting non-zero (typical case: file does not exist) is treated as
  "no embedded config"; the merge proceeds with an empty image map.
- Any *non-cat* failure (container died, podman daemon error) propagates as
  `OutrigError::Process` like every other podman call.
- `cat` succeeding produces UTF-8 bytes; non-UTF-8 content is a hard parse
  error.

The reason for `podman exec ... cat` over the alternatives:

- `podman create` + `podman cp` + `podman rm` requires its own lifecycle and
  can't reuse the already-running session container.
- Image labels (`buildah inspect`) would force a flat string encoding for
  what's naturally a nested TOML table; we'd reinvent escaping.

### Merge precedence

Per server name:

1. Embedded `[mcp]` from `/etc/outrig/container.toml` (image base).
2. `[containers.<name>.mcp.<server>]` from `config.toml` (override).

`config.toml` wins on key collisions. The override is **whole-entry**, not
field-level: a `config.toml` entry replaces the image's matching entry
wholesale. This matches the existing config-merge style in
`src/config/merge.rs` (whole-table replacement, no deep merge of inner
fields) and avoids the "did the user mean to drop or keep my env?"
ambiguity. Servers that appear in only one source come through unchanged.

A user who wants to fully delegate to the image leaves
`[containers.<name>.mcp]` empty (or omits it entirely) -- the migration story
is "delete the mcp block, ship the image".

### Where the merge happens

Just before the MCP startup loop in `src/cli/run.rs:201`:

```rust
let embedded = read_embedded_container(&container).await?; // EmbeddedContainerConfig
let merged   = merge_mcp(embedded.mcp, &container_cfg.mcp);
for (mcp_name, spec) in &merged {
    let client = McpClient::connect_via_podman_exec(container, spec, mcp_name, log_dir).await?;
    ...
}
```

`read_embedded_container` and `merge_mcp` live in a new
`src/container/embedded.rs`. The `container_cfg.mcp` field stays the same
shape; the merge produces a freshly-allocated `BTreeMap<String,
McpServerSpec>` for the loop to consume.

`outrig mcp` (`plan/done/0040-outrig-mcp-wire-subcommand.md`, shipped)
inherits the same merge automatically because it uses the same MCP startup
path. This task threads the read+merge into the `outrig mcp` startup as
well as `outrig run`.

### Errors

- TOML parse failure of `/etc/outrig/container.toml`:
  `OutrigError::EmbeddedContainerParse { container, source }` -- a malformed
  file is a build-time bug worth surfacing loudly.
- Server name collision where the image and `config.toml` both define the
  same name: silently use the `config.toml` version. This is "override", not
  "conflict".
- Server name in either source not matching `^[a-zA-Z][a-zA-Z0-9_-]*$`: same
  validation as today, framed at the source it came from (image -> embedded
  parse error; config -> config validation error).

## Deliverables

- `src/container/embedded.rs` (new) -- `EmbeddedContainerConfig` struct
  (`#[serde(default)]` `mcp` field of type `BTreeMap<String, McpServerSpec>`,
  with `serde(deny_unknown_fields)` deliberately *not* set so unknown
  top-level tables are ignored), `read_embedded_container(&Container)
  -> Result<EmbeddedContainerConfig>`, and `merge_mcp(image:
  BTreeMap<String, McpServerSpec>, config: &BTreeMap<String, McpServerSpec>)
  -> BTreeMap<String, McpServerSpec>`. Unit tests covering the merge cases.
- `src/cli/run.rs:201` -- call read + merge before the existing MCP loop;
  loop iterates over `merged` instead of `container_cfg.mcp`.
- `src/error.rs` -- `EmbeddedContainerParse { container: String, source:
  toml::de::Error }` variant, framed in the same style as the existing
  `McpEnvResolveFailed` / config-load errors.
- `doc/concepts/mcp-servers.md` -- new "Embedding MCP config in the image"
  subsection between "Declaring servers" and "Lifecycle". Show the
  `container.toml` snippet, document the merge semantics (config overrides
  image, whole-entry), explain when to use which (shared image -> embed;
  per-repo overrides -> config). Cross-link from
  `doc/reference/config.md` `[containers.<name>.mcp]` schema row.
- `tests/embedded_container.rs` -- integration tests covering:
  1. Image-only: empty config block, image provides everything; expected
     servers boot.
  2. Override: image and config both define `fs`; the spawned argv matches
     config, not image.
  3. Additive: image provides `fs`+`shell`, config adds `build`; all three
     boot.
  4. Missing file: container with no `/etc/outrig/container.toml` falls back
     to config silently; no warning, no error.
  5. Malformed TOML: surfaces as `EmbeddedContainerParse`.
  6. Forward-compat: a `container.toml` containing an unknown top-level
     table parses successfully and `[mcp]` is read normally.
- A test-fixture container-context that bakes
  `/etc/outrig/container.toml` (under `tests/fixtures/` or via a
  `Dockerfile` heredoc snippet) so tests 1-3, 5, 6 can exercise the real
  podman path. Test 4 reuses an existing fixture without the file.

## Acceptance

- `cargo test` (including the new integration suite) and `cargo clippy
  --all-targets` pass.
- `outrig run` against a config with no `[containers.<name>.mcp]` block but
  an image that ships `/etc/outrig/container.toml` boots the image's MCP
  set.
- `outrig run` with both image and config entries shows config winning on
  conflicts and image entries surviving as additions; the per-server `[outrig]
  startup line lists the merged set.
- An existing image with no `/etc/outrig/container.toml` continues to work
  unchanged -- backward compat by construction.
- `outrig mcp` (shipped in `plan/done/0040`) inherits the merged view.

## Sub-decisions

- **`--show-merged-mcp` (or similar) debug surface** that runs the
  read+merge and prints the result, for users debugging "why is this server
  starting with the wrong command?". Useful but not required for v0.
- **Logging the merge result at `info`** when `outrig run` starts (e.g.
  `merged mcp: 3 from image, 1 override from config`). Probably yes;
  matches the verbosity of existing `[outrig]` startup lines.
- **Future sections in `container.toml`** -- e.g. a `[workspace]` hint, an
  `[env]` block of default overlays, a `[labels]` table for tooling. v0 only
  reads `[mcp]` and ignores the rest (forward-compat). Each new section is
  its own follow-up spec.
- **Drop-in directory `/etc/outrig/container.d/*.toml`**. Adds complexity;
  punt unless a real need shows up (multi-stage images that compose MCP
  sets).
- **Reading `container.toml` *before* container start** (via `podman create`
  + `podman cp`) so the merged view exists during config validation, not
  just at MCP-spawn time. Probably not worth the lifecycle complexity for
  v0; revisit only if a feature lands that needs the merged view earlier
  (e.g. a `--show-config` subcommand).

## Dependencies

- None hard. `plan/done/0040-outrig-mcp-wire-subcommand.md` has shipped,
  so this task threads the read+merge through both `outrig run` and
  `outrig mcp`. `plan/todo/0054-outrig-mcp-attach.md` will inherit the
  merged behavior automatically when it lands, because it reuses the same
  MCP startup path.
