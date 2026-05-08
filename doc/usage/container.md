# `outrig container`

`outrig container` groups commands that manage container-configs (the named Dockerfile +
MCP-server bundles that host the agent's tools). In v0 only `outrig container add` is implemented;
the rest of the group (`container ls`, `container rm`) is reserved for later.

## `outrig container add`

`outrig container add` scaffolds a new container-config. It writes a Dockerfile under
`.agents/outrig/containers/<name>/Dockerfile` and appends matching `[containers.<name>]` and
`[containers.<name>.mcp]` blocks to your repo's `config.toml`.

Run it any time you want to add a container-config -- e.g., a `planning` config alongside
the `<repo>-standard` one [`outrig init`](init.md) creates by default. `init` calls
`container add` in a loop for the first (and any further) containers you create during
initial setup.

### Bootstrapping a fresh repo

If you run `outrig container add` in a directory without an `.agents/outrig/config.toml`
(neither here nor in any parent), outrig prompts before scaffolding:

```
[outrig] no .agents/outrig/config.toml found in /path/to/repo or any parent.
? Configure outrig in this directory now? [Y/n]:
```

Answering `y` walks the same repo-config prompts that [`outrig init`](init.md) uses
(workspace, default agent, preamble), then continues with the `container add` flow.
Answering `n` exits with the same error a strict `find` would have produced -- run
`outrig init` later when you're ready.

### Synopsis

```
outrig container add [<name>] [--force]
```

| Argument / flag | Default  | Description                                                 |
|-----------------|----------|-------------------------------------------------------------|
| `<name>`        | prompted | Container-config name (becomes `[containers.<name>]`).      |
| `--force`       | off      | Overwrite existing Dockerfile/config entries for this name. |

### Run it

Every prompt shows the default in `[default: ...]`; press Enter to accept it. Type `?` and
Enter at any prompt for an explanation of what's being asked plus the available options.
The default container-config name is `<repo-folder>-standard` (kebab-cased), so the example
below assumes a `hello-outrig` repo.

```sh
$ cd hello-outrig
$ outrig container add
? Container-config name [default: hello-outrig-standard]:
? Base image [default: debian:bookworm-slim]:
? Language toolchains [default: ]: rust, node
? MCP servers [default: fs]:

[outrig] wrote .agents/outrig/containers/hello-outrig-standard/Dockerfile
[outrig] added [containers.hello-outrig-standard] block to .agents/outrig/config.toml
[outrig] added [containers.hello-outrig-standard.mcp] entries: fs

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

  See: doc/usage/container.md#known-toolchains

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
editing the `[containers.<name>.mcp]` block directly -- see
[Concepts -> MCP Servers](../concepts/mcp-servers.md).
A shell MCP server is the usual next tool for coding containers; choose a package, install it in
the Dockerfile, and declare its command in `[containers.<name>.mcp]`.

> **TODO: Incomplete** -- the list of "known MCP servers" will grow as the ecosystem does.
> Anything not listed here you install in the Dockerfile by hand.

### When templates do not fit

The prompt flow is intentionally small. If you need a container with a database, internal SDK,
unlisted MCP server, or other custom package set, use
[`outrig mcp self`](ai-assisted-design.md). It gives an MCP-capable AI tool the OutRig docs,
config schema, suggested tools, and advisory validators so it can propose a Dockerfile and
matching `[containers.<name>]` block without being limited to the built-in template menu.

### What gets written

`.agents/outrig/containers/hello-outrig-standard/Dockerfile` (excerpt):

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
[containers.hello-outrig-standard]
dockerfile = ".agents/outrig/containers/hello-outrig-standard/Dockerfile"
context    = ".agents/outrig/containers/hello-outrig-standard"

  [containers.hello-outrig-standard.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
```

### Re-running

Without `--force`, outrig refuses if either the Dockerfile path or the config block already
exists for that name:

```
$ outrig container add hello-outrig-standard
error: .agents/outrig/containers/hello-outrig-standard/Dockerfile
       already exists; pass --force to overwrite.
```

With `--force`, the Dockerfile is replaced and the `[containers.<name>]` block is rewritten in
place (preserving surrounding TOML).

## See also

- [outrig init](init.md) -- runs `container add` in a loop as the last step of initial setup.
- [Concepts -> Containers](../concepts/containers.md) -- Dockerfile conventions and named
  container-configs.
- [Concepts -> MCP Servers](../concepts/mcp-servers.md) -- the MCP servers `container add`
  scaffolds.
- [AI-assisted design](ai-assisted-design.md) -- design a custom container-config with
  `outrig mcp self`.
- [Reference -> Config](../reference/config.md) -- the `[containers.<name>]` schema.
