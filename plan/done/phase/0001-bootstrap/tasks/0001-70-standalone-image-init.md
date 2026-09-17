# 0070 -- `outrig image init` for standalone image projects

## Context

`outrig image add` scaffolds a repo-local image-config under
`.agents/outrig/images/<name>/` and appends `[images.<name>]` to the repo config.
Standalone toolset images are different: they are independent projects whose
output is a reusable container image. They need their own project scaffold, not
a repo config mutation.

## Goal

Add a noninteractive `outrig image init` command that creates a standalone image
project with a Dockerfile, embedded `image.toml`, and a README.

## Deliverables

- Add `outrig image init [DIR]` under the existing `outrig image` command group.
- Generate these files in the target directory:
  - `Dockerfile`
  - `image.toml`
  - `README.md`
- Default the project name from `DIR` when provided, otherwise from the current
  directory name.
- Write bare `image.ref = "<name>"` by default, e.g. `rust-dev`.
- Use the existing Debian slim base-image conventions by default.
- Install and declare the filesystem MCP server by default:
  - Dockerfile installs `@modelcontextprotocol/server-filesystem`.
  - `[mcp].fs` command is `["mcp-server-filesystem", "/workspace"]`.
- Ensure the generated Dockerfile follows OutRig image conventions:
  - Includes packages needed for runtime UID/GID bootstrap.
  - Does not set `USER`.
  - Ends with `CMD ["sleep", "infinity"]`.
  - Copies `image.toml` into `/etc/outrig/image.toml`.
- Refuse to overwrite existing files unless an explicit `--force` flag is
  supplied.
- Document `outrig image init`, the generated files, and a minimal consuming
  repo config that uses `image-name` with no `[images.<name>.mcp]` block.

## Acceptance

- `outrig image init rust-dev` creates `rust-dev/Dockerfile`,
  `rust-dev/image.toml`, and `rust-dev/README.md`.
- The generated `image.toml` passes `validate_image_toml`.
- The generated Dockerfile passes the existing Dockerfile advisory validator.
- Running the command twice without `--force` reports the existing-file conflict.
- `--force` replaces only the generated files for that project.
- Unit or integration tests cover default naming, generated contents, and
  idempotency.

## Dependencies

- **Hard: 0069**. The scaffold writes the canonical standalone `image.toml`
  shape and embeds it at `/etc/outrig/image.toml`.

## Decisions

- **Name derivation** is just `target_dir.file_name()`. `Path::components`
  normalizes away `.` (so `init .` and the no-argument form use the current
  directory's basename) but preserves a trailing `..`, for which -- and for the
  filesystem root -- `file_name()` is `None`; we refuse with a clear error
  rather than guess. The name must match `^[a-zA-Z][a-zA-Z0-9_-]*$` (a small
  char check, no `regex` dependency), guaranteeing a clean `ref` token and a
  valid TOML bare key for the README's `[images.<name>]`.
- **Dockerfile generation reuses `render::render(DebianBookwormSlim, [], [Fs])`**
  and splices the `COPY image.toml /etc/outrig/image.toml` line in before the
  footer's unique `WORKDIR /workspace` token, rather than adding a dedicated
  `render_standalone()`. This keeps a single Dockerfile-assembly source of
  truth; a unit test pins that the token appears exactly once. `/simplify`
  generated an independent alternative that converged on the same approach;
  the reviewer kept this one for its consistency with `image add` (relative-path
  output via the shared `display_rel` idiom) and lazy rendering.
- **`image.toml` omits `[build]`** (it defaults to the sibling `Dockerfile`
  with context `.`) and uses the full `fs = { command = [...] }` form for
  parity with `image add` output.
- **The conflict message says "already present"** (number-neutral) instead of
  `add`'s "already exists", because `init` can report several colliding files in
  one error.
- **Validators reached via `pub(crate)`**: `mcp_self::validate` was widened from
  `mod` to `pub(crate) mod` so init's in-crate unit tests call the real
  `validate_image_toml` / `validate_dockerfile` without widening the public API.
- **The `outrig image build` forward reference is kept** in the generated README
  and the stderr next-step hint: 0071 is the committed next task and its
  acceptance builds exactly this scaffold.
