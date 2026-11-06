# Building without the musl target

A `view = "primary"` sidecar runs its MCP server against the primary container's filesystem
(see [MCP Servers](../concepts/mcp-servers.md)). That works through `outrig-enter`, a small
launcher compiled while outrig itself is built and embedded in it. This page covers how the
launcher is built, what each way of building it needs, and what to do when none works.

## Why the launcher is static

`outrig-enter` is the sidecar's entrypoint. It runs inside the sidecar image, joins the primary
container's mount namespace, and then starts the server. OutRig does not control that image, so
the launcher cannot need anything from it. A dynamically linked launcher would need the image
to supply a loader and a C library at least as new as the one it was linked against. Alpine and
other musl-based images, `scratch`, and distroless-static images have no glibc loader at all,
and a launcher linked against glibc 2.34 or newer does not start on an older glibc, such as
Debian 11's, Ubuntu 20.04's, or UBI 8's. A statically linked launcher needs none of that, so it
runs in every image.

## How the build chooses

outrig's build tries two ways to get a static launcher, in this order:

1. **The musl target.** The launcher is compiled for `x86_64-unknown-linux-musl` or
   `aarch64-unknown-linux-musl`, whichever matches the architecture being built. That needs
   only the Rust target, which `rustup target add` installs: musl targets bring their own C
   library and need no C toolchain. CI and published releases use this build.
2. **The build machine's own target, linked statically.** Without the musl target, or when its
   build fails, the same source is compiled for the host's own target with
   `-C target-feature=+crt-static`. On a glibc host that links glibc into the launcher, which
   then runs in every image just as the musl build does. It needs the host's C compiler driver
   (`cc`) and its static C library, `libc.a`:
   - Debian and Ubuntu ship `libc.a` in `libc6-dev`, which any Rust build that links against
     glibc already needs.
   - Fedora ships it in `glibc-static`, which is not installed by default.

   This step applies to a native build only. When the target architecture is not the build
   machine's, the host's own target would produce a launcher for the wrong architecture, and
   the musl target is the only way.

When neither works, outrig still builds, without the launcher. `view = "primary"` sidecars are
then refused before any container is created, and the
[built-in default image-config](../reference/config.md#the-built-in-default-image-config) falls
back: its `fs` server becomes a read-write bind mount of the workspace, and its `shell` server
is dropped.

## Telling which one you got

The build prints a cargo warning whenever it does not use the musl target. With the fallback in
use, the warning names the host target the launcher was built for. Without any launcher, it
says why each attempt failed and that `view="primary"` sidecars will be unavailable.

Cargo shows that warning only for a crate you build from a local path -- a clone of this
repository, or `cargo install --path` -- and hides it for a crate from crates.io, which is what
`cargo install outrig-cli` builds, unless the build fails or you pass `-vv`.

At run time, a build without the launcher says so when a session needs it:

```
error: this outrig was built without the filesystem-view helper
build detail: <why each build failed, and what to install>
help: see https://tgockel.github.io/outrig/usage/no-musl.html
```

A session on the built-in default image-config prints a note at startup instead, beginning
``warning: this build has no `outrig-enter` launcher``.

## Getting the launcher

Install the musl target for your architecture:

```sh
rustup target add x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
```

Then rebuild. A build from a local path notices the new target by itself, so a plain
`cargo build` picks it up. `cargo install` builds from scratch each time, so run
`cargo install --force outrig-cli`.

Or install the host's static C library (on Fedora, `sudo dnf install glibc-static`) and rebuild
outrig from scratch. Nothing the build watches changes when a system package is installed, so
run `cargo clean -p outrig` before rebuilding from a local path, or use
`cargo install --force outrig-cli`.

## Requiring the musl build

Setting `OUTRIG_REQUIRE_ENTER` to any value turns a missing musl build into a build error: the
fallback is not tried, and the build does not go on without a launcher. CI and the release
checklist set it, so that a launcher which stopped compiling for musl fails there rather than
passing on a different build.
