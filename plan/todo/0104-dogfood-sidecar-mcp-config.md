# 0104 -- This repo's own image embeds the MCP servers it should be attaching

## Context

`.agents/outrig/config.toml` declares `fs`, `git` and `shell` as exec-stdio servers in the
primary container, so `.agents/outrig/images/outrig-standard/Dockerfile` carries `nodejs`,
`npm`, `python3-pip` and `python3-venv` plus this layer solely to host them:

```dockerfile
RUN npm install -g @modelcontextprotocol/server-filesystem \
 && pip install --break-system-packages mcp-server-git mcp-shell-server
```

Two package managers and four apt packages, in an image whose reason to exist is a Rust
toolchain. This is precisely the shape sidecar placement was built to remove, and OutRig's own
repo is the one config that should be demonstrating the answer rather than the problem.

It is also the only place the feature gets exercised outside the gated e2e suite. Nothing in
day-to-day use of this repo touches `[sidecars]`, `view = "primary"`, entrypoint-stdio, or the
`image-name` pull path -- so nothing catches a regression in them until a release does.

`shell` is what made this hard, and 0102 is what makes it possible. Its `ALLOW_COMMANDS` list
is the *primary's* toolchain (`cargo`, `mdbook`, `mdbook-mermaid`, `lychee`, `rg`), so the only
sidecar shape that reaches it is `view = "primary"` -- which until 0102 ran its payload as root
in the primary's user namespace, meaning every `target/` directory it wrote would have been
owned by a host subuid. With 0102 landed the payload runs as the session user, and with 0103
landed a published image's bare `ENTRYPOINT` resolves, so `fs` can use a stock image unmodified.

## Goal

`.agents/outrig/images/outrig-standard/Dockerfile` installs no MCP server. All three servers run
as `view = "primary"` sidecars, and working in this repo exercises that path every day.

## Deliverables

- `.agents/outrig/config.toml`: replace the three `[images.outrig-standard.mcp]` entries with
  the inline anonymous entrypoint form carrying `view = "primary"` -- the shape
  `doc/concepts/mcp-servers.md:136` documents. Sketch, with image refs settled by the probe
  below:

  ```toml
  [images.mcp-filesystem]
  image-name = "docker.io/mcp/filesystem:latest"

  [images.mcp-shell]
  dockerfile = ".agents/outrig/images/mcp-shell/Dockerfile"
  context    = ".agents/outrig/images/mcp-shell"

    [images.outrig-standard.mcp]
    fs  = { image = "mcp-filesystem", view = "primary", args = ["/workspace"] }
    git = { image = "mcp-git", view = "primary", args = ["--repository", "/workspace"] }

    [images.outrig-standard.mcp.shell]
    image = "mcp-shell"
    view  = "primary"

      # ALLOW_COMMANDS / CARGO_HOME / RUSTUP_HOME / PATH carry over verbatim.
      [images.outrig-standard.mcp.shell.env]
  ```

  That `env` block moves across unchanged, and the reason is worth a comment in the config:
  every value in it names a *primary* path, which is correct, because the commands the shell
  server spawns resolve in the primary's view. Only the interpreter comes from the graft, and
  it finds its own prefix from its grafted path.

- Settle each image by probing, not by assuming. `outrig image inspect --remote <ref>`
  (`doc/usage/image.md:393`) reads a published image's `ENTRYPOINT` without pulling layers:
  - `fs` -- published `docker.io/mcp/filesystem:latest`, wrapped in an `image-name`
    image-config so it *pulls*. A raw ref in an `image`/`[sidecars].image` position resolves
    `--pull=never` (`crates/outrig/src/image.rs:301-305`): that position is the fallback after
    `[images.<name>]` lookup fails, and a typo must not reach a registry. The `image-name`
    shape is the explicit pull path (`ImageSourceRef::Image` -> `pull_image_logged`,
    `crates/outrig/src/image.rs:484-499`).
  - `git` -- probe `docker.io/mcp/git`. Wrap it the same way if its ENTRYPOINT resolves; else
    a local `.agents/outrig/images/mcp-git/Dockerfile` (`python:3-slim` +
    `pip install mcp-server-git`, `ENTRYPOINT ["/usr/local/bin/python3", "-m", "mcp_server_git"]`).
    The sidecar image needs no `git` binary either way -- it runs in the primary's mount
    namespace, which has one.
  - `shell` -- no published image exists; local `.agents/outrig/images/mcp-shell/Dockerfile`
    (`python:3-slim` + `pip install mcp-shell-server`,
    `ENTRYPOINT ["/usr/local/bin/python3", "-m", "mcp_shell_server"]`).
  - For either locally built image, confirm the package actually ships a `__main__` before
    settling on `-m`. The ENTRYPOINT program has to be an absolute path to an ELF binary: a
    `#!` console script is not something `elf_interp` can classify, so `mcp-server-git` and
    `mcp-shell-server` cannot be named directly even though they are on `PATH`.
  - Pin versions in any Dockerfile written here. The layer being deleted is unpinned, unlike
    the `cargo install --locked` above it; do not carry that forward.

- `.agents/outrig/images/outrig-standard/Dockerfile`: drop `nodejs`, `npm`, `python3-pip`,
  `python3-venv` and the whole `npm install`/`pip install` layer. Keep `python3` -- it is in
  `ALLOW_COMMANDS` as an agent-usable tool, not an MCP dependency. Do **not** reintroduce
  `passwd`/`shadow`; `617e19ff` removed them deliberately once the host began writing
  `/etc/passwd` through setns.

- Same Dockerfile: add `rustup target add x86_64-unknown-linux-musl` beside the existing
  `rustup component add clippy rustfmt`. Without it an `outrig` built *inside* this container
  embeds an empty launcher -- `crates/outrig/build.rs` degrades silently by design -- and its
  own `view = "primary"` sessions fail at start. This config creates that trap, so this task
  closes it.

- `CONTRIBUTING.md`: state the same target requirement for host builds, since running this
  repo's config now depends on the launcher being present.

## Acceptance

- `outrig run` in this repo starts with three sidecar containers, and `/sidecar list` shows
  all three.
- Driven through the agent: `fs` lists `/workspace`; `git` reports status on the real repo;
  `shell` runs `cargo --version` and `cargo fmt --check`.
- Files the sidecars write into the workspace are owned by the invoking user. This is 0102's
  payoff and the thing that would silently regress without it -- assert it explicitly rather
  than trusting that the servers appeared to work.
- `outrig mcp show-merged` renders all three with their sidecar placement.
- The primary image builds with no `pip` and no `npm` present.

## Design note

All three servers land on `view = "primary"` rather than the lower-privilege mix of
`workspace = "rw"` exec-stdio sidecars for `fs`/`git` and `view = "primary"` only for `shell`.
Once 0102 removes the capability and ownership asymmetry, `view = "primary"` is not the more
privileged option -- it is the one that needs no bind mount, cannot disagree with the primary
about paths, and reads the same tree the agent's shell sees. Uniformity is also the point: one
shape, exercised three times a session, in the repo that ships the feature.

## Dependencies

- **0102**, for the payload to run as the session user; without it every file these servers
  write is subuid-owned.
- **0103**, for `docker.io/mcp/filesystem`'s bare `ENTRYPOINT` to resolve; without it `fs`
  needs a locally built image instead of the published one.

## See also

- `doc/concepts/mcp-servers.md` -- the placement shapes and the `view = "primary"` section.
- `doc/usage/image.md` -- `outrig image inspect --remote`, and the curated `fs`/`git` recipes
  whose install commands this task stops using.
- `plan/next/user-image-library.md` -- where these per-server image-configs would eventually
  move, once a user-level image library exists.
