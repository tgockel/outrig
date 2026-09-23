# The musl target is a manual step before `cargo build` works

## Context

`CONTRIBUTING.md` opens with `rustup target add <arch>-unknown-linux-musl`, because
`crates/outrig/build.rs` compiles the `outrig-enter` launcher for it. Without the target the build
succeeds with a warning and an empty launcher, and every `view = "primary"` sidecar fails at
session start.

The maintainer's rule, set while landing 0003-01, is that `cargo build` and `cargo run` work with
no external steps. 0003-01 made the static CPython follow it: `build.rs` downloads, verifies, and
embeds it. The launcher's target is the one prerequisite left.

## Goal

A fresh clone builds a complete OutRig -- launcher included -- with `cargo build` and nothing
before it.

## Design forks

1. **`rust-toolchain.toml` naming the targets -- likely.** rustup installs the listed `targets`
   on the first cargo invocation in the tree, which is the idiomatic answer. It also pins a
   channel for everyone, which interacts with the public-API job's pinned nightly
   (`scripts/check-public-api.py`) and with anyone building from a packaged crate, where the
   file does not apply.
2. **`build.rs` running `rustup target add` itself.** Works from a package too, but a build
   script that modifies the toolchain is invasive, and fails where rustup is not the toolchain
   manager.
3. **Building the launcher without the musl sysroot.** It uses `std` today (`launcher.rs:70`),
   so this means porting it to `no_std` and raw syscalls -- the most work, and the only option
   that removes the requirement rather than automating it.

## Dependencies

None.
