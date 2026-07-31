# Make Windows a host OutRig can actually be built on

## Context

`crates/outrig` does not compile for a Windows target. Since the `compile_error!` at
`src/lib.rs:11-27` it says so in one line instead of a screenful, but nothing has changed
underneath. Two obstacles are Windows-specific and neither is large:

- **The `nix` dependency.** `crates/outrig/Cargo.toml:35` names it unconditionally, and `nix`
  binds \*nix APIs -- there is no Windows build of it at all. That makes it a manifest change
  rather than a use-site one: it has to move behind `[target.'cfg(unix)'.dependencies]`, which
  in turn requires every `nix::` caller to sit inside a `cfg`.
- **Two unconditional `std::os::unix` uses** outside test code:
  `src/container/enter/mod.rs:18` (`PermissionsExt`, for the `0755` on the materialized helper)
  and `crates/outrig-cli/src/session.rs:142` (`symlink`). Two more are test-only
  (`src/mcp.rs:674`, `src/image.rs:1010`) and so block `cargo test` rather than `cargo build`.
  `src/mcp.rs:456-458` and `crates/outrig-cli/src/cli/mcp.rs:21-22` are already `cfg(unix)`-gated
  and show the shape the rest should take.

Two things that look like obstacles are not. `src/container/enter/launcher.rs:54` also names
`std::os::unix`, but `build.rs` compiles that file for a Linux musl target and it is never part
of the library. And the unix-socket listener already degrades correctly: `cli/mcp.rs:440` gates
`serve_unix_http_transport` on `cfg(unix)`, and the non-Unix arm returns "unix listen addresses
require a Unix platform" (`cli/mcp.rs:462-468`).

Everything else is shared with `plan/next/macos-host-support.md`, for a reason worth stating
plainly: **the axis is local versus remote podman, not OS.**

## The shared half

podman on Windows runs the engine in a WSL2 VM with a native client, exactly as podman machine
does on macOS. The code already models that. `container/mod.rs:90`'s
`podman_service_is_remote()` probes `{{.Host.ServiceIsRemote}}`, and
`direct_bootstrap_supported()` (`:81`) turns the host-side bootstrap off when it is true. So the
machinery a Windows host could not run in any case -- `nsfork`'s `fork`/`setns`/`SCM_RIGHTS`,
`container::namespace`'s join of `/proc/<pid>/ns/*`, `network`'s `nsenter`/`nft` interceptor --
is *already inert at runtime* there, with the `podman exec` bootstrap carrying the session. The
defect is only that it is compiled unconditionally.

`plan/next/macos-host-support.md` owns that `cfg(target_os = "linux")` boundary and should land
first; this entry is the smaller half that sits on top of it.

## Why it might matter

Weakly, and that is worth being honest about up front: WSL2 already gives a Windows developer a
working OutRig, and it is an ordinary Linux build with no special path through the codebase.
That is a real argument for capping the ambition here, or for not doing this at all. What a
native build would buy is a Windows developer running `cargo install outrig-cli` in their own
shell against Windows-side checkouts, rather than keeping the repo inside the WSL2 filesystem.

## Goal

`cargo build` succeeds for `x86_64-pc-windows-msvc`, and every capability that cannot work there
is absent by construction rather than broken at runtime.

## Deliverables

- **`nix` behind `cfg(unix)`** in `crates/outrig/Cargo.toml`, with its callers gated to match.
  This follows the macOS boundary work rather than duplicating it.
- **The two non-test `std::os::unix` uses replaced.** `session.rs:142`'s `symlink` needs a
  portable equivalent or a `cfg`; `enter/mod.rs:18`'s chmod may simply disappear -- see the
  `podman cp` item below.
- **Volume-source path translation.** Every `-v` from `append_launch_flags`
  (`container/mod.rs:1182`) names a client path: the workspace bind, and
  `-v <helper_host>:/outrig-enter:ro` (`:1215`). Across a VM boundary those must be paths the
  *engine* can see. Establish how much podman's own Windows client already translates before
  designing anything on top of it -- this is a measurement, not a design decision.
- **`--volume` syntax.** `crates/outrig-cli/src/cli/volume_arg.rs:1-7` parses
  `HOST:CONTAINER[:ro|rw]`, so a host path cannot contain `:` and `C:\...` is inexpressible. The
  comment there already records this as deliberate for "a Linux/podman tool"; a Windows host
  needs that stance revisited.
- **A route for the helper that is not a client-side bind.** Sidecars already `podman create`
  (`container/mod.rs:1151`) and only later `podman start --attach --interactive`
  (`src/mcp.rs:171`), so there is a window for `podman cp <helper> <container>:/outrig-enter`.
  It streams over the remote API, works on a created-but-unstarted container, and `--entrypoint`
  is resolved at start rather than create. That would also retire the `PermissionsExt` chmod.
- **Docs.** `README.md`, `CONTRIBUTING.md`, `doc/quickstart.md`, and
  `crates/outrig-cli/src/mcp_self/docs/concepts/mcp-servers.md` all say native Windows is
  unsupported and point at WSL2; each needs the new story.

Note that `-v /proc/<pid>/ns:/target-ns:ro` (`container/mod.rs:1211`) is *not* affected:
`primary_pid` comes from `podman inspect --format {{.State.Pid}}`, which is already an
engine-side PID resolving against an engine-side path.

## Acceptance

- `cargo check -p outrig --target x86_64-pc-windows-msvc` passes in CI, not just locally.
- A Windows build that reaches a Linux-only capability says so with a message naming the
  capability, in the shape `OutrigError::FilesystemHelperUnavailable` already uses.
- Linux behavior is unchanged: no new `cfg` makes a Linux build take a different path.
- The `compile_error!` at `src/lib.rs:11-27` is narrowed or deleted, whichever the boundary work
  leaves correct.

## Design forks

1. **Supported target or merely a compiling one -- Open.** The same fork
   `plan/next/macos-host-support.md` records. "It builds and the ordinary run path works" is far
   cheaper than "every feature works", and WSL2 covers the gap meanwhile. The docs have to say
   which promise is being made.

2. **Where path translation lives -- Open.** Leaning on podman's client translation is the small
   change; normalizing to engine coordinates inside `append_bind_mount`
   (`container/mod.rs:1272`) is the one that stays honest when the engine is reached over SSH
   rather than through podman machine.

3. **Whether `view = "primary"` is offered at all -- Open.** It needs the primary's namespaces,
   which live in the VM. `plan/next/primary-view-remote-podman.md` proposes refusing it outright
   while the service is remote; that is the conservative default this entry can build on.

## Dependencies

- `plan/next/macos-host-support.md` -- owns the `cfg(target_os = "linux")` boundary and the
  `view = "primary"` question. Land it first, or this duplicates it.
- `plan/next/primary-view-remote-podman.md` -- supplies the interim refusal that keeps a remote
  engine from reaching a code path built for a local one.
