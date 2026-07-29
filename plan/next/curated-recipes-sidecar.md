# Curated `fs`/`git` recipes still install into the primary

## Problem

`outrig init`'s design prompt tells the model to install MCP servers into the primary image
with the package manager:

```
RUN npm install -g @modelcontextprotocol/server-filesystem
RUN pip install --break-system-packages mcp-server-git
```

(`crates/outrig-cli/src/cli/design_prompt.rs:250-251`, `:284-285`, and the "Known MCP
servers" list at `doc/usage/image.md:106-120`.)

That is the shape 0104 removed from this repo's own config: it drags `nodejs`/`npm` or
`python3-pip`/`python3-venv` into an image whose reason to exist is something else, and it
installs unpinned. A generated image-config therefore starts life with the problem the
sidecar placement feature exists to solve, while the repo that ships the feature does not.

## Sketch

Emit `view = "primary"` sidecars instead:

```toml
[images.mcp-filesystem]
image-name = "docker.io/mcp/filesystem:latest"

  [images.<name>.mcp]
  fs = { image = "mcp-filesystem", view = "primary", args = ["/workspace"] }
```

Points that 0104 settled and this work would inherit:

- A published image is usable directly only when its `ENTRYPOINT` program is an ELF binary.
  `docker.io/mcp/filesystem` qualifies (`node`); `docker.io/mcp/git` does not -- its
  entrypoint is a `#!` console script, so `git` needs a small local Dockerfile that names the
  interpreter (see `.agents/outrig/images/mcp-git/Dockerfile`).
- A raw registry ref in an `image` position never pulls. The `[images.<name>] image-name` wrap
  is what makes it pull, so the generated config needs the extra block.
- `view = "primary"` needs the `outrig-enter` launcher, so `init` should say what happens when
  the musl target was missing at build time rather than letting the session fail at start.

## Open questions

Whether the recipes should offer both shapes (in-image for users who want one container, a
sidecar otherwise) or switch outright. The in-image form is still the only one that works
when the launcher is unavailable.

Whether the wrapper image for `git` should exist at all. `elf.rs` refuses a `#!` payload on
the grounds that its interpreter line would resolve inside the target namespace -- but the
dynamic path already solves that exact problem for `PT_INTERP` by naming the loader as
`{graft}{interp}`. Teaching the launcher to read a shebang and graft-prefix it the same way
would make `docker.io/mcp/git` usable unmodified, and would delete one of the two Dockerfiles
0104 had to write. Worth costing before this task designs around the restriction.
