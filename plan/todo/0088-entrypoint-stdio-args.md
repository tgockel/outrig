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

`org.outrig.mcp` labels carry `McpServerSpec` values, so an image can declare its own `args`.
That falls out of the existing serialization with no extra work, but `crates/outrig/tests/
embedded_image.rs` should pin it so a label round-trip cannot silently drop the key.

## Validation

| Case                                      | Result                                           |
|-------------------------------------------|--------------------------------------------------|
| `args` with `image`, no `command`         | accepted -- the entrypoint-stdio form            |
| `args` with `command`                     | rejected -- exec-stdio already has a full argv   |
| `args` with `sidecar`                     | rejected -- `sidecar` implies exec-stdio         |
| `args` with neither `image` nor `sidecar` | rejected -- a primary-placed server is exec-stdio |
| `args = []`                               | accepted, elided on serialization                |

## Acceptance

- A config declaring only `fs = { image = "...", args = ["/workspace"] }` starts a server that
  received the argument, verified against a fixture image whose ENTRYPOINT fails without one.
  `crates/outrig/tests/fixtures/mcp-entrypoint/Dockerfile` is the existing fixture to extend.
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

## See also

- `doc/concepts/mcp-servers.md` -- placement forms and the entrypoint-stdio contract.
- `plan/done/0080-sidecar-entrypoint-stdio.md` -- where the entrypoint form landed, and why
  container lifetime equals server lifetime.
- `plan/todo/0090-primary-view-sidecars.md` -- the consumer that needs this to demo.
