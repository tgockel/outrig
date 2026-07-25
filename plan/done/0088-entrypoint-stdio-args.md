# 0088 -- Arguments for entrypoint-stdio MCP servers

## Context

An entrypoint-stdio MCP server is declared by naming an image and no command:

```toml
[images.coding.mcp]
fs = { image = "docker.io/mcp/filesystem:latest" }
```

The image's ENTRYPOINT is the server, and container lifetime equals server lifetime
(`McpServerSpec::is_entrypoint_stdio`, `crates/outrig/src/config/mod.rs:1047-1049`). What the
form cannot do is pass the server an argument. `build_podman_create_cmd`
(`crates/outrig/src/container/mod.rs:661-682`) ends with

```rust
cmd.args(["--interactive", "--rm"]).arg(image.0.as_str())
```

-- no trailing argv, by construction. `env` is reachable because it renders as `--env` flags;
positional arguments have no surface at all.

That rules out most real MCP server images. `docker.io/mcp/filesystem`, the reference
filesystem server and the one most likely to be a user's first sidecar, takes its allowed
directories as positional arguments and refuses to start without at least one. Today the only
way to reach it is to abandon the entrypoint form and write out the full in-image command as an
exec-stdio entry, which requires knowing the image's internal layout
(`/usr/local/bin/node /app/dist/index.js`) -- exactly the coupling the entrypoint form exists to
avoid.

The gap stands on its own, but it also blocks 0090: the `view = "primary"` demo is an unmodified
third-party MCP image served from config alone, and that image needs an argument.

## Goal

Let an entrypoint-stdio MCP server take positional arguments from config, so naming an image is
enough to run it.

## Deliverables

- An `args` key on `McpServerSpec::Full` (`crates/outrig/src/config/mod.rs:982-1005`), defaulted
  and elided when empty:

  ```toml
  [images.coding.mcp]
  fs = { image = "docker.io/mcp/filesystem:latest", args = ["/workspace"] }
  ```

- `args` threaded to `build_podman_create_cmd` and appended after the image ref, alongside the
  existing `env` parameter. Both are entrypoint-stdio-only, so they belong together rather than
  on `ContainerLaunchSpec`, which is shared with `podman run`.
- Validation in `crates/outrig/src/config/validate.rs`: `args` requires `image`, and is rejected
  next to `command` or `sidecar`. The error names the server and the image-config.
- An accessor next to `has_command` / `is_entrypoint_stdio`, and `args` carried through
  `merge_sidecar_labels`' spec rewrite (`crates/outrig/src/container/sidecar.rs:223-238`) so
  `show-merged` reports what will actually run.
- The library facade: `McpServerSpec` reaches `Outrig` callers, so whatever `outrig_.rs` and
  `crates/outrig-cli/src/cli/session_setup.rs` pass to the create path carries `args` too. 0087
  recorded that the CLI bypasses `SecuritySpec` and reads `ImageConfig` directly at both launch
  sites; check for the same shape here rather than assuming the facade covers it.
- Docs in the same commit: `doc/reference/config.md` (the `[images.<name>.mcp]` table and the
  validation-rules list) and `doc/concepts/mcp-servers.md` (the entrypoint-stdio section).

## Runtime behavior

`podman create IMAGE [ARG...]` appends to an exec-form ENTRYPOINT and *replaces* CMD. So for an
image built as

```dockerfile
ENTRYPOINT ["node", "/app/dist/index.js"]
```

`args = ["/workspace"]` yields `node /app/dist/index.js /workspace`, which is what a user
expects. For an image with no ENTRYPOINT that puts its server in CMD, the same `args` *replace*
the CMD outright and the server never starts. That is podman's rule, not something to paper
over: document it in `doc/concepts/mcp-servers.md` next to the key, and say plainly that `args`
is for images whose server is an ENTRYPOINT.

~~`org.outrig.mcp` labels carry `McpServerSpec` values, so an image can declare its own `args`.
That falls out of the existing serialization with no extra work~~ -- **wrong, see Decisions**.
`args` is rejected in labels, and the rejection is what gets pinned.

## Validation

Revised during execution; see Decisions. `sidecar` without `command` became the named
entrypoint-host form rather than an error, so `args` is accepted beside it.

| Case                                      | Result                                            |
|-------------------------------------------|---------------------------------------------------|
| `args` with `image`, no `command`         | accepted -- the inline entrypoint-stdio form      |
| `args` with `sidecar`, no `command`       | accepted -- the named entrypoint-host form        |
| `args` with `command`                     | rejected -- exec-stdio already has a full argv    |
| `args` with neither `image` nor `sidecar` | rejected -- a primary-placed server is exec-stdio |
| `args` on both the entry and its block    | rejected -- declare the arguments in one place    |
| `args` on a block hosting no entrypoint   | rejected -- nothing runs that argv                |
| An entrypoint host with a second server   | rejected -- the container process is the server   |
| An entrypoint host with `start = manual`  | rejected -- lifetime is the server's              |
| `args` in an `org.outrig.mcp` label       | rejected -- labels declare exec-stdio servers     |
| `args = []`                               | accepted, elided on serialization                 |

## Acceptance

- A config declaring only `fs = { image = "...", args = ["/workspace"] }` starts a server that
  received the argument, verified against a fixture image whose ENTRYPOINT fails without one.
  `crates/outrig/tests/fixtures/mcp-entrypoint/Dockerfile` is the existing fixture to extend.
  (The "fails without one" part had to be *added* to the fixture -- see Decisions.)
- `podman create` argv places `args` last, after the image ref, and an entry without `args`
  produces a byte-identical argument vector to the one produced before this change.
- `crates/outrig/tests/config_merge.rs` covers each rejected combination in the table above.
- A `Full` entry with `args` round-trips through `show-merged` without collapsing into `Short`.
- An `org.outrig.mcp` label carrying `args` survives the label -> plan -> serialization path.

## Open questions

- **Whether `args` should also apply to exec-stdio.** It would be redundant with `command` and
  invites two ways to say one thing. Rejected here; revisit only if a caller wants to append to
  a label-declared command without restating it.
- **Shell-style splitting.** `args` is a string list, never a string to be split. No quoting
  rules, matching `command`.

## Dependencies

None.

## Decisions

Calls made during execution (2026-07-24):

- **Sidecars moved to a top-level `[sidecars.<sc>]` map**, folded into this task rather than
  queued. Every other cross-referenced entity in the config is already top-level and named
  (`agents.<n>.model` -> `[models.<n>]`, and so on); sidecars were the sole exception, and the
  nesting was never argued -- the program spec that introduced it records only the consequence.
  Doing it here means `args` never ships on a surface that is about to move. One block is now
  shared by any number of image-configs, and global-config sidecars fall out for free, since
  `merge` already extends every top-level map global-then-repo by name.
- **Instantiation follows reference**: a sidecar starts only when some `[mcp]` entry names it.
  Declaring-is-instantiating cannot survive the move -- a personal toolbox in the *user's*
  global config would otherwise start in every repo. Two capabilities go away with it: a
  sidecar hosting no MCP servers, and one whose servers came only from its image's
  `org.outrig.mcp` label. The label is outrig's own, so only an image built for outrig carries
  one; the pattern was thin enough not to keep an opt-in key for.
- **`args` reachability is checked across the whole config, not per image-config.** A shared
  block can legitimately be an entrypoint host for one image-config and an exec-stdio host for
  another, so `SidecarArgsWithoutEntrypoint` fires only when *no* image-config hosts an
  entrypoint server in it. The single-server and `start = "auto"` rules stay per image-config,
  because which kind of host a block is follows from the referencing entry.

- **Scope grew to cover named sidecars as entrypoint hosts.** As drafted, `args` was reachable
  only through the inline `image` form, because a named block launches as `<image> sleep
  infinity` (`build_podman_run_cmd`) and `sidecar` without `command` was rejected. That leaves
  no way to give an entrypoint server a workspace view, mounts, or its own security policy --
  and 0090 needs exactly that, since `view` lives on the block. So **entrypoint mode is now
  decided by the absence of `command`**, extending the rule that already governed the inline
  form: `is_entrypoint_stdio` became `!has_command() && (image().is_some() ||
  sidecar().is_some())`. It is the single transport classifier, so placement planning, MCP
  connect, and the library facade's rejection all widened at one site.
  `McpSidecarRequiresCommand` is removed.
- **`args` lives on both `McpServerSpec::Full` and `SidecarConfig`**, and setting both for one
  container is an error rather than a merge. The block is where a shared/named container's
  argv belongs; the entry is what the inline one-liner needs. `SessionMcpPlan::entrypoint_args`
  is the one place that picks between them.
- **An entrypoint host serves exactly one server and must be `start = "auto"`.** The container
  process *is* the server, so a second server has nothing to exec into, and `/sidecar add`
  (`launch_declared_sidecar` -> `start_one_sidecar`) only knows the `podman run` path. Both are
  config errors rather than half-supported runtime paths.
- **Entrypoint hosts never bootstrap, even with mounts.** `sidecar_needs_bootstrap` returns
  false for them outright. Bootstrap runs over `podman exec`, and there is no window between
  `podman create` and the `podman start --attach` that runs the server. The image's own `USER`
  applies to `workspace` and `mounts`, which is now documented in `concepts/workspace.md`.
- **`sidecar_launch_base` took over the workspace/mount fill** from `start_one_sidecar`, so
  named entrypoint hosts honor both -- `podman create` accepts `-v`/`-w` exactly as `podman
  run` does. Anonymous sidecars declare neither and land on the same empty defaults as before.
- **Label handling keyed on a new `sidecar_honors_labels` predicate** (`!anonymous &&
  entrypoint_server_in().is_none()`) rather than `!anonymous`, and the Phase-A label *read*
  uses the same predicate as the Phase-B merge. Otherwise a named entrypoint host's image would
  be inspected for a label nothing consumes, which can newly fail a session on a malformed one.
- **The `args`-in-labels acceptance criterion was wrong and is inverted.** This file claimed
  labels carry `args` "with no extra work". They cannot: `args` requires `image`/`sidecar` and
  `parse_mcp_table` already rejects both as `PlacementInLabel`, while an `args` with neither
  would parse and be silently dropped. A label declares exec-stdio servers in the image that
  carries it, whose arguments belong in `command`. Added `ArgsInLabel` to both the label and
  standalone-`image.toml` validators, checked before the empty-command rule so the error names
  the real problem, and pinned the rejection instead of a round-trip.
- **The e2e fixture now requires an argument**: `entry.sh` passes `"$@"` through and exits 64
  on an empty argv, at the cost of adding `args = ["/tmp"]` to the three existing entrypoint
  tests. The no-args create path is covered by the argv unit tests instead, where the
  byte-identical regression guard lives.

  The explicit guard is there because this task's premise turned out to be wrong. Measured
  against the built fixture: `mcp-server-filesystem` with no directory does **not** exit -- it
  starts and waits for the client to supply roots over the MCP protocol, which outrig's proxy
  never does. Dropped `args` would still have failed the assertions, but as a denied
  `list_directory` ("path outside allowed directories") rather than as a missing argument.
  Both signals now fire, and the faster-to-read one fires first.

## See also

- `doc/concepts/mcp-servers.md` -- placement forms and the entrypoint-stdio contract.
- `plan/done/0080-sidecar-entrypoint-stdio.md` -- where the entrypoint form landed, and why
  container lifetime equals server lifetime.
- `plan/todo/0090-primary-view-sidecars.md` -- the consumer that needs this to demo.
