# `outrig image`

`outrig image` groups commands for the container images that host an agent's tools.
`outrig image add` scaffolds a repo-local image-config (a named Dockerfile + MCP-server
bundle); `outrig image init` scaffolds a standalone image project whose build output is a
reusable image, `outrig image build` builds and validates that project, and
`outrig image inspect` reads a local image's declared labels. The rest of the group
(`image ls`, `image rm`) is reserved for later.

## `outrig image add`

`outrig image add` scaffolds a new image-config. It writes a Dockerfile under
`.agents/outrig/images/<name>/Dockerfile` and appends matching `[images.<name>]` and
`[images.<name>.mcp]` blocks to your repo's `config.toml`.

Run it any time you want to add an image-config -- e.g., a `planning` config alongside
the `<repo>-standard` one [`outrig init`](init.md) creates by default. `init` calls
`image add` in a loop for the first (and any further) image-configs you create during
initial setup.

### Bootstrapping a fresh repo

If you run `outrig image add` in a directory without an `.agents/outrig/config.toml`
(neither here nor in any parent), outrig prompts before scaffolding:

```
[outrig] no .agents/outrig/config.toml found in /path/to/repo or any parent.
? Configure outrig in this directory now? [Y/n]:
```

Answering `y` walks the same repo-config prompts that [`outrig init`](init.md) uses
(workspace, default agent, preamble), then continues with the `image add` flow.
Answering `n` exits with the same error a strict `find` would have produced -- run
`outrig init` later when you're ready.

### Synopsis

```
outrig image add [<name>] [--force]
```

| Argument / flag | Default  | Description                                                 |
|-----------------|----------|-------------------------------------------------------------|
| `<name>`        | prompted | Image-config name (becomes `[images.<name>]`).              |
| `--force`       | off      | Overwrite existing Dockerfile/config entries for this name. |

### Run it

Every prompt shows the default in `[default: ...]`; press Enter to accept it. Type `?` and
Enter at any prompt for an explanation of what's being asked plus the available options.
The default image-config name is `<repo-folder>-standard` (kebab-cased), so the example
below assumes a `hello-outrig` repo.

```sh
$ cd hello-outrig
$ outrig image add
? Image-config name [default: hello-outrig-standard]:
? Base image [default: debian:bookworm-slim]:
? Language toolchains [default: ]: rust, node
? MCP servers [default: fs]:

[outrig] wrote .agents/outrig/images/hello-outrig-standard/Dockerfile
[outrig] added [images.hello-outrig-standard] block to .agents/outrig/config.toml
[outrig] added [images.hello-outrig-standard.mcp] entries: fs

Next: try `outrig build` to verify the image builds, then `outrig run`.
```

The prompts are intentionally limited -- the goal is a known-good starting point you can edit
by hand, not an exhaustive Dockerfile generator.

### Help at any prompt

```
? Language toolchains [default: ]: ?

  Pick zero or more language toolchains to install in the image. The Dockerfile
  template adds the corresponding install steps; you can edit the file afterwards.

  rust    rustup + stable toolchain (cargo, rustfmt, clippy).
  node    Node 20 LTS via NodeSource.
  python  CPython 3.12 with pip and venv.
  go      Go 1.22.
  none    Just the base image -- nothing extra installed.

  See: https://tgockel.github.io/outrig/usage/image.html#known-toolchains

? Language toolchains [default: ]:
```

### Known toolchains

The toolchain prompt offers curated options that cover the common cases:

| Choice   | What gets installed                                               |
|----------|-------------------------------------------------------------------|
| `rust`   | `rustup` + the stable toolchain, `cargo`, `rustfmt`, `clippy`.    |
| `node`   | Node 20 LTS via the base image's package manager (or NodeSource). |
| `python` | CPython 3.12 with `pip` and `venv`.                               |
| `go`     | Go 1.22.                                                          |
| `none`   | Just the base image.                                              |

You can pick more than one. The Dockerfile is a starting point -- edit it freely afterwards.

### Known MCP servers

- **`fs`** -- installs `@modelcontextprotocol/server-filesystem` from npm; default
  `[mcp]` entry: `{ command = ["mcp-server-filesystem", "/workspace"] }`.
- **`git`** -- installs `mcp-server-git` from PyPI; default `[mcp]` entry:
  `{ command = ["mcp-server-git", "--repository", "/workspace"] }`.

`fs` is the default. Add `git` for repo-aware tools, or wire up additional servers by
editing the `[images.<name>.mcp]` block directly -- see
[Concepts -> MCP Servers](../concepts/mcp-servers.md).
A shell MCP server is the usual next tool for coding image-configs; choose a package, install it
in the Dockerfile, and declare its command in `[images.<name>.mcp]`.

> **TODO: Incomplete** -- the list of "known MCP servers" will grow as the ecosystem does.
> Anything not listed here you install in the Dockerfile by hand.

### When templates do not fit

The prompt flow is intentionally small. If you need an image with a database, internal SDK,
unlisted MCP server, or other custom package set, use
[`outrig mcp self`](ai-assisted-design.md). It gives an MCP-capable AI tool the OutRig docs,
config schema, suggested tools, and advisory validators so it can propose a Dockerfile and
matching `[images.<name>]` block without being limited to the built-in template menu.

### What gets written

`.agents/outrig/images/hello-outrig-standard/Dockerfile` (excerpt):

```Dockerfile
FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git build-essential passwd \
 && rm -rf /var/lib/apt/lists/*

# rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
       | sh -s -- -y --default-toolchain stable --profile default
ENV PATH=/root/.cargo/bin:$PATH

# node toolchain
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/*

# MCP servers
RUN npm install -g @modelcontextprotocol/server-filesystem

WORKDIR /workspace
CMD ["sleep", "infinity"]
```

The Dockerfile is generic -- no `USER` directive, no hard-coded UID. outrig sets up a user
matching your host UID/GID at run time (see
[Concepts -> Workspace](../concepts/workspace.md#uidgid-runtime-user-mapping)). The
`passwd` package keeps `useradd`/`groupadd` available for that bootstrap step.

Appended to `.agents/outrig/config.toml`:

```toml
[images.hello-outrig-standard]
dockerfile = ".agents/outrig/images/hello-outrig-standard/Dockerfile"
context    = ".agents/outrig/images/hello-outrig-standard"

  [images.hello-outrig-standard.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
```

### Re-running

Without `--force`, outrig refuses if either the Dockerfile path or the config block already
exists for that name:

```
$ outrig image add hello-outrig-standard
error: .agents/outrig/images/hello-outrig-standard/Dockerfile
       already exists; pass --force to overwrite.
```

With `--force`, the Dockerfile is replaced and the `[images.<name>]` block is rewritten in
place (preserving surrounding TOML).

## outrig image init

`outrig image init` scaffolds a *standalone* image project: a self-contained directory whose
build output is a reusable container image. Unlike `image add`, it writes no repo config --
it creates three files and nothing else:

- `Dockerfile` -- a Debian-slim image with the filesystem MCP server, ending in the standard
  `CMD ["sleep", "infinity"]`. It installs the tool binaries; it does not copy OutRig config.
- `image.toml` -- the standalone image authoring config: `[image].ref` plus a `[mcp]` table.
  `outrig image build` validates this file and stamps the `[mcp]` table into the image's OCI
  labels, which outrig reads back at session startup and merges with any repo
  `[images.<name>.mcp]`.
- `README.md` -- how to build the image and reference it from a repo.

The command is noninteractive -- there are no prompts. The project name (used as `[image].ref`)
defaults to the target directory's name.

### Synopsis

```
outrig image init [<dir>] [--force]
```

| Argument / flag | Default     | Description                                          |
|-----------------|-------------|------------------------------------------------------|
| `<dir>`         | current dir | Project directory; its name becomes the image ref.  |
| `--force`       | off         | Overwrite the generated files if they already exist. |

The directory name must match `^[a-zA-Z][a-zA-Z0-9_-]*$` (e.g. `rust-dev`). A target that
resolves to no usable name is rejected; `.` and the no-argument form use the current
directory's name.

### What gets written

```sh
$ outrig image init rust-dev
[outrig] wrote rust-dev/Dockerfile
[outrig] wrote rust-dev/image.toml
[outrig] wrote rust-dev/README.md
[outrig] next: build this image with `outrig image build`
```

`rust-dev/image.toml`:

```toml
[image]
ref = "rust-dev"

[mcp]
fs = { command = ["mcp-server-filesystem", "/workspace"] }
```

`[build]` is omitted, so the build defaults to the sibling `Dockerfile` with context `.`.

### Consuming the image from a repo

Once built, reference the image from a repo's `config.toml` with `image-name`. Because the
MCP server declarations are stamped into image labels, the consuming block needs no
`[images.<name>.mcp]`:

```toml
[images.rust-dev]
image-name = "rust-dev"
```

That label-vs-repo split is the key difference from `image add`, whose repo-local blocks carry
their own `[images.<name>.mcp]` entries. See
[Concepts -> Containers](../concepts/containers.md#using-a-pre-built-image) for the
`image-name` shape.

### Re-running

Without `--force`, `init` refuses if any of the three files already exists, naming the
conflicts:

```
$ outrig image init rust-dev
error: rust-dev/Dockerfile, rust-dev/image.toml, rust-dev/README.md
       already present; pass --force to overwrite.
```

`--force` regenerates the three files and leaves anything else in the directory untouched.

## outrig image build

`outrig image build` builds a standalone image project (the output of `outrig image init`) and
validates that the result is a working OutRig toolset image. It is the standalone-project
counterpart to [`outrig build`](build.md), which builds repo-local image-configs.

It reads the project's `image.toml`, serializes it into OCI labels, builds the declared
`Dockerfile` with buildah, and tags the result with the `[image].ref` from `image.toml` --
stamping those labels onto the image. Then it always validates the built image, and -- unless
`--no-test` -- live-tests every MCP server the image declares.

### Synopsis

```
outrig image build [<dir>] [--tag <ref>] [--no-test] [--no-cache]
```

| Argument / flag | Default       | Description                                           |
|-----------------|---------------|-------------------------------------------------------|
| `<dir>`         | current dir   | Project directory holding `image.toml`.               |
| `--tag <ref>`   | `[image].ref` | Build tag override; does not edit `image.toml`.       |
| `--no-test`     | off           | Skip the live MCP test; still validates `image.toml`. |
| `--no-cache`    | off           | Force a clean build; passed through to buildah.       |

### What it does

```sh
$ outrig image build rust-dev
[outrig] building image rust-dev
[outrig]   dockerfile: Dockerfile
[outrig]   context:    .
... buildah build output ...
[outrig] image ready
[outrig] image config validated (1 mcp server)
[outrig] mcp fs: initialized (12 tools)
[outrig] image ok
```

After the build, outrig reads the stamped `org.outrig.mcp` label back off the built image and
validates it (every server name well-formed, every command non-empty). By default it then starts
the image, and for each declared MCP server initializes it and calls `tools/list`, reporting the
tool count per server. The image is built locally; nothing is pushed.

### Failure modes

The command exits nonzero -- printing `error: ...` -- in these cases:

- **`image.toml` missing, malformed, or missing required fields** (no `[image].ref`, an empty or
  invalid `[mcp]` table, a partial `[build]`): caught before the build runs.
- **The stamped `org.outrig.mcp` label is missing or malformed**: read back off the built image
  and validated after the build, even with `--no-test`.
- **A declared MCP server cannot initialize or return `tools/list`**: caught during the live
  test. `--no-test` skips this check (the per-server stderr is captured in the error message).

### `--no-test`

`--no-test` skips *only* the live MCP server test. The stamped `org.outrig.mcp` label is still
read back from the built image and validated, so a missing or malformed config still fails. Use
it to verify a build quickly, or in environments where starting the servers is not wanted.

### `--tag` and `--no-cache`

`--tag <ref>` tags the build output as `<ref>` for that invocation instead of the `[image].ref`
in `image.toml`; the file is never rewritten. Short refs are passed to buildah verbatim, so both
`rust-dev` and `rust-dev:0.1.0` work.

Unlike repo-local `outrig build` -- which tags `<image-config-name>:<content-hash>` and skips
the build on a cache hit -- a standalone build tags a stable, caller-named ref with no content
hash, so there is no project-level cache to skip: `--no-cache` only forwards `--no-cache` to
buildah to force a clean build.

## outrig image inspect

`outrig image inspect` prints the OutRig config labels declared by a local image. It is
read-only: it checks the local image store, reads labels with `podman image inspect`, and never
pulls, creates a container, or starts an MCP server. Live initialization and `tools/list` checks
belong to [`outrig image build`](#outrig-image-build).

### Synopsis

```
outrig image inspect <ref>
```

| Argument | Description                            |
|----------|----------------------------------------|
| `<ref>`  | Local image ref to inspect. Never pulled. |

### Output

The command writes only the inspect result to stdout. Diagnostics and errors go to stderr.

```sh
$ outrig image inspect rust-dev
image: rust-dev
description: Rust tooling
version: 0.1.0
tags: ["rust","build"]
mcp:
  fs:
    command: ["mcp-server-filesystem","/workspace"]
  db:
    command: ["db-mcp"]
    env:
      TOKEN: "${DB_TOKEN}"
```

`description`, `version`, and `tags` appear only when the image carries those labels. If a
local image has no `org.outrig.mcp` label, inspect still prints the image ref and any metadata
labels it finds, then omits the `mcp:` section.

### Failure modes

The command exits nonzero -- printing `error: ...` -- in these cases:

- **The image is not present locally**: inspect reports a local-only not-found error and does not
  attempt a pull.
- **A declared OutRig label is malformed**: invalid `org.outrig.tags` or `org.outrig.mcp` values
  fail before anything is started.

## See also

- [outrig init](init.md) -- runs `image add` in a loop as the last step of initial setup.
- [Concepts -> Containers](../concepts/containers.md) -- Dockerfile conventions and named
  image-configs.
- [Concepts -> MCP Servers](../concepts/mcp-servers.md) -- the MCP servers `image add`
  scaffolds.
- [AI-assisted design](ai-assisted-design.md) -- design a custom image-config with
  `outrig mcp self`.
- [Reference -> Config](../reference/config.md) -- the `[images.<name>]` schema.
