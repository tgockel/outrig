# `outrig container`

> **TODO: Incomplete** -- every command and behavior on this page describes outrig's intended
> behavior; the implementation isn't ready yet.

`outrig container` groups commands that manage container-configs (the named Dockerfile +
MCP-server bundles agents run inside). In v0 only `outrig container add` is implemented;
the rest of the group (`container ls`, `container rm`) is reserved for later.

## `outrig container add`

`outrig container add` scaffolds a new container-config. It writes a Dockerfile under
`.agents/outrig/containers/<name>/Dockerfile` and appends matching `[containers.<name>]` and
`[containers.<name>.mcp]` blocks to your repo's `config.toml`.

Run it any time you want to add a container-config -- e.g., a `planning` config alongside
`coding`. [`outrig init`](init.md) calls `container add` in a loop for the first (and any
further) containers you create during initial setup.

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

```sh
$ outrig container add
? Container-config name [default: coding]:
? Base image [default: debian:bookworm-slim]:
? Language toolchains, comma-separated [default: ]: rust, node
? MCP servers, comma-separated [default: fs, shell]:

[outrig] wrote .agents/outrig/containers/coding/Dockerfile
[outrig] added [containers.coding] block to .agents/outrig/config.toml
[outrig] added [containers.coding.mcp] entries: fs, shell

Next: try `outrig build` to verify the image builds, then `outrig run`.
```

The prompts are intentionally limited -- the goal is a known-good starting point you can edit
by hand, not an exhaustive Dockerfile generator.

### Help at any prompt

```
? Language toolchains, comma-separated [default: ]: ?

  Pick zero or more language toolchains to install in the image. The Dockerfile
  template adds the corresponding install steps; you can edit the file afterwards.

  rust    rustup + stable toolchain (cargo, rustfmt, clippy).
  node    Node 20 LTS via NodeSource.
  python  CPython 3.12 with pip and venv.
  go      Go 1.22.
  none    Just the base image -- nothing extra installed.

  See: doc/usage/container.md#known-toolchains

? Language toolchains, comma-separated [default: ]:
```

### Known toolchains

The toolchain prompt offers presets that cover the common cases:

| Choice   | What gets installed                                               |
|----------|-------------------------------------------------------------------|
| `rust`   | `rustup` + the stable toolchain, `cargo`, `rustfmt`, `clippy`.    |
| `node`   | Node 20 LTS via the base image's package manager (or NodeSource). |
| `python` | CPython 3.12 with `pip` and `venv`.                               |
| `go`     | Go 1.22.                                                          |
| `none`   | Just the base image.                                              |

You can pick more than one. The Dockerfile is a starting point -- edit it freely afterwards.

### Known MCP servers

| Choice  | Package installed                         | Default `[mcp]` entry                                      |
|---------|-------------------------------------------|------------------------------------------------------------|
| `fs`    | `@modelcontextprotocol/server-filesystem` | `["mcp-server-filesystem", "/workspace"]`                  |
| `shell` | `mcp-server-shell`                        | `["bash", "-lc", "exec mcp-server-shell"]`                 |
| `git`   | `mcp-server-git`                          | `{ command = ["mcp-server-git", "--repo", "/workspace"] }` |

Picking `fs` and `shell` covers most coding workflows. Add more later by editing the
`[containers.<name>.mcp]` block directly -- see
[Concepts -> MCP Servers](../concepts/mcp-servers.md).

> **TODO: Incomplete** -- the catalogue of "known MCP servers" will grow as the ecosystem does.
> Anything not listed here you install in the Dockerfile by hand.

### What gets written

`.agents/outrig/containers/coding/Dockerfile` (excerpt):

```Dockerfile
FROM docker.io/library/debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates curl git build-essential passwd \
 && rm -rf /var/lib/apt/lists/*

# rust toolchain
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
       | sh -s -- -y --default-toolchain stable

# node toolchain
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
 && apt-get install -y nodejs \
 && rm -rf /var/lib/apt/lists/*

# MCP servers
RUN npm install -g @modelcontextprotocol/server-filesystem \
 && npm install -g mcp-server-shell

WORKDIR /workspace
CMD ["sleep", "infinity"]
```

The Dockerfile is generic -- no `USER` directive, no hard-coded UID. outrig sets up a user
matching your host UID/GID at run time (see
[Concepts -> Workspace](../concepts/workspace.md#uidgid-runtime-user-mapping)). The
`passwd` package keeps `useradd`/`groupadd` available for that bootstrap step.

Appended to `.agents/outrig/config.toml`:

```toml
[containers.coding]
dockerfile = ".agents/outrig/containers/coding/Dockerfile"
context    = ".agents/outrig/containers/coding"

  [containers.coding.mcp]
  fs    = { command = ["mcp-server-filesystem", "/workspace"] }
  shell = ["bash", "-lc", "exec mcp-server-shell"]
```

### Re-running

Without `--force`, outrig refuses if either the Dockerfile path or the config block already
exists for that name:

```
$ outrig container add coding
error: .agents/outrig/containers/coding/Dockerfile already exists; pass --force to overwrite.
```

With `--force`, the Dockerfile is replaced and the `[containers.<name>]` block is rewritten in
place (preserving surrounding TOML).

## See also

- [outrig init](init.md) -- runs `container add` in a loop as the last step of initial setup.
- [Concepts -> Containers](../concepts/containers.md) -- Dockerfile conventions and named
  container-configs.
- [Concepts -> MCP Servers](../concepts/mcp-servers.md) -- the MCP servers `container add`
  scaffolds.
- [Reference -> Config](../reference/config.md) -- the `[containers.<name>]` schema.
