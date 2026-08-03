# Make macOS a host OutRig can actually be built on

## Context

`crates/outrig` does not compile for an Apple target, and nothing in the repo says so until you
try. Three modules reach for Linux-only libc surface with no `cfg` between them and the crate
root:

- `src/nsfork.rs:208` -- `libc::setns` via `nix::libc`.
- `src/network.rs:976,983` -- `libc::CLONE_NEWUSER`, `libc::CLONE_NEWNET`.
- `src/container/namespace.rs:304,316` -- the same two.

`setns` is declared only under `libc/src/unix/linux_like/{linux,android}` and the `CLONE_NEW*`
constants only under `linux_like` and `fuchsia`, so an Apple target fails at name resolution.
`src/lib.rs:19-20` declares `network` and `nsfork` unconditionally, so there is no configuration
in which the crate skips them.

This went unnoticed because no job compiles anything for macOS -- the same gap
`plan/next/ci-configuration-coverage.md` records as its highest-value CI item. A round of docs
briefly claimed macOS was supported on the strength of `build.rs` selecting the helper triple by
architecture rather than OS; that rule is right, but it is a statement about the *helper*, which
runs inside the container, not about the host.

## Why it might matter

macOS is where a large share of container-using developers work, and podman machine already
supplies the Linux VM that everything below the host process needs. The pieces that genuinely
require Linux are the ones that manipulate the *primary's* namespaces from the host -- the
network interceptor and the user bootstrap -- not the ordinary run path.

## Goal

`cargo build` succeeds on `aarch64-apple-darwin`, and every feature that cannot work there is
absent by construction rather than broken at runtime.

## Deliverables

- **A Linux-only boundary.** `nsfork`, `network`'s namespace-entering half, and
  `container::namespace` behind `cfg(target_os = "linux")`, with the callers gated to match.
- **A runtime user for a non-Linux host.** This entry used to point at the `podman exec`
  bootstrap as the fallback for a namespace-join failure. That fallback is gone: the host-side
  write through `container::namespace` is the only bootstrap, and every failure in it is now
  fatal. So a macOS host currently has *no* path to a runtime user, not a slower one, and this
  task has to supply one -- most likely the `podman cp` route that
  `plan/next/primary-view-remote-podman.md` fork 1 sketches for the helper binary, which
  streams over the remote API and does not care where the client's filesystem is.
  `plan/next/windows-host-support.md` needs the same answer.
- **A decision on the network interceptor.** It is the one subsystem with no non-namespace
  implementation. Either it is a Linux-only capability that a macOS build reports as
  unavailable the way `view = "primary"` does today, or it needs a podman-machine-side design.
- **A decision on `view = "primary"`.** The helper cross-compiles fine from a macOS host, but
  it is launched with `--userns=container:<target>` and needs the primary's namespaces, which
  live in the VM. Whether that composes through podman machine is the open question.
- **CI.** A `macos-latest` check job, which `plan/next/ci-configuration-coverage.md` wants
  anyway for the `local-llm,metal` dependency block. One job covers both.
- **Docs.** `README.md`, `CONTRIBUTING.md`, `doc/quickstart.md`, and
  `crates/outrig-cli/src/mcp_self/docs/concepts/mcp-servers.md` all state Linux-only today;
  each needs the new story.

## Acceptance

- `cargo check --target aarch64-apple-darwin -p outrig` passes in CI, not just locally.
- A macOS build that hits a Linux-only capability says so with a message naming the capability,
  in the shape `OutrigError::FilesystemHelperUnavailable` already uses.
- Linux behavior is unchanged: no new `cfg` makes a Linux build take a different path.

## Design forks

1. **Where the boundary goes -- Open.** Per-module `cfg` is the small change; a
   `platform::linux` submodule with a trait the rest of the crate talks to is the one that
   stays legible once a second capability needs the same treatment.

2. **Whether macOS is a supported target or a compiling one -- Open.** "It builds and the
   ordinary run path works" is a much cheaper promise than "every feature works", and is
   probably the right first step. The docs have to say which one is being made.

## Dependencies

- None, but it overlaps `plan/next/ci-configuration-coverage.md`'s macOS job; land them
  together or the job goes in red.
