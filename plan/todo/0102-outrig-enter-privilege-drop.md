# 0102 -- A `view = "primary"` payload runs as root, not as the session user

## Context

Every other MCP server OutRig starts runs as the person who launched the session. Exec-stdio
servers get there through `Container::exec`, which passes `--user=<uid>:<gid>`
(`crates/outrig/src/container/mod.rs:968`) after the host-side user bootstrap. A
`view = "primary"` sidecar does not, and cannot as things stand:

- `view = "primary"` is entrypoint-stdio only, so the container process *is* the server.
- `SessionMcpPlan::sidecar_needs_bootstrap` short-circuits entrypoint hosts
  (`crates/outrig/src/container/sidecar.rs:298`) -- bootstrap runs over `podman exec`, and
  there is no window for it between `podman create` and the `podman start --attach` that runs
  the entrypoint. So the image's own `USER` applies.
- That `USER` is effectively forced to root: `outrig-enter` needs `CAP_SYS_ADMIN` for
  `open_tree`/`setns`/`move_mount` and `CAP_SYS_PTRACE` to open the target's nsfs file.

The sidecar runs with `--userns=container:<primary>` over a `--userns=keep-id` primary, so
container uid 0 maps to a host *subuid*. Everything such a server writes into the workspace --
`target/`, `.git/`, `/workspace/.cargo` -- lands owned by an id the invoking user does not have
and cannot `chown` back without `podman unshare`. And because the payload is uid 0 with those
caps in its bounding set, every process it spawns inherits `CAP_SYS_ADMIN` too: an MCP server
that shells out hands the capability to whatever it runs.

Today this is latent -- the shipped `view = "primary"` examples are read-only filesystem
servers. It stops being latent the moment a server that writes, or that spawns commands, is
placed this way, which is exactly what `plan/todo/0104-dogfood-sidecar-mcp-config.md` does.

The fix belongs in the launcher. `outrig-enter` holds those privileges *by necessity*, but only
until the graft is in place. Nothing after `chdir` needs them.

## Goal

A `view = "primary"` payload runs as the session's uid/gid with an empty capability set, so it
is indistinguishable from an exec-stdio server in what it may do and what it may own. The
launcher keeps the privileges only across the window that requires them.

## Deliverables

- `crates/outrig/src/container/enter/launcher.rs`: accept optional `--uid N` / `--gid N`
  alongside the existing `--target` / `--ns-file` / `--graft` / `--cwd` flags. When present,
  drop privileges after `chdir` and before either exec path -- the `ElfKind::Static`
  `execveat` and the `ElfKind::Dynamic` loader `execv` alike -- via `setgroups(0, NULL)`,
  then `setresgid`, then `setresuid`. Declare the three new libc symbols the way the file
  already declares `setns`/`mount`/`execv`. `die()` on any failure; continuing as root after
  being asked to drop is the one outcome worse than not starting.
- No explicit `capset`: the kernel clears the permitted, effective and ambient sets on a uid
  transition away from 0. Say so in a comment, because the absence of a capability call is
  otherwise the first thing a reader will flag.
- Keep both flags optional. Omitting them preserves today's behavior, which keeps the launcher
  independently runnable -- it is a standalone binary with a documented argv contract, not only
  an OutRig implementation detail.
- Extend the module header's argv-contract paragraph. It already warns that the launcher and
  `build_primary_view_argv` must change together; the new invariant to record is the *ordering*
  one: `open(program)`, `open_tree`, `open(ns)`, `setns`, `unshare`, `mount`, `move_mount` and
  `chdir` all need the privileges, and `prog_fd` is opened before the drop, so `execveat` from
  it still works afterwards.
- `crates/outrig/src/container/sidecar.rs`: `build_primary_view_argv` takes uid/gid and emits
  `--uid`/`--gid` within the flag block ahead of `--`; `entrypoint_create_args` threads them
  through. Take them as parameters rather than calling `getuid()` inside the builder, so the
  argv unit tests stay hermetic and the function keeps its single responsibility.
- Update both production callers to pass the session values, which each already has to hand
  via `Container::uid`/`gid` (`crates/outrig/src/container/mod.rs:633`):
  `crates/outrig-cli/src/cli/session_setup.rs:1098` and `crates/outrig/src/outrig_.rs:1042`.
  Config path and library path must produce identical argv, which is the property
  `entrypoint_create_args` exists to hold.
- Docs. Three places state the current behavior and now need a `view = "primary"` carve-out --
  the *launcher* takes `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE`, the payload does not, and the image's
  `USER` no longer governs what the payload may own:
  `doc/concepts/mcp-servers.md` (the entrypoint-host `USER` note and the "real posture change"
  paragraph), `doc/concepts/containers.md` (the sidecar create flags), and
  `doc/concepts/mcp-trust-model.md`.

## Acceptance

- Unit: `build_primary_view_argv` emits `--uid`/`--gid` in the flag block, and
  `entrypoint_create_args_fills_in_the_primary_view_bind_targets` still holds as an equivalence
  between the two constructions.
- e2e, `crates/outrig-cli/tests/primary_view_e2e.rs`: a file the sidecar writes into the
  primary's workspace is owned by the invoking uid. Every existing assertion still passes --
  the sidecar reads the primary's tree, sees `/usr/local/cargo/bin/cargo`, its graft stays
  invisible to the primary, and killing the primary reaps it.
- A privileged operation attempted by the payload fails, proving the capabilities are gone
  rather than merely unused.
- Omitting both flags reproduces today's argv exactly.

## Design note

Dropping in the launcher beats a `podman create --user` on the sidecar, which is the other
obvious option. `--user` would apply to `outrig-enter` itself, and podman would then have to
grant `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` to a non-root user for the setns to work at all -- a
strictly wider grant, and one that depends on runtime ambient-capability behavior rather than on
anything OutRig controls. Dropping after the graft narrows the privileged window to the few
syscalls that genuinely need it, in the one binary this repo compiles for exactly that purpose.

Always dropping, rather than adding a config key to opt in, is the right default: a server that
wants root over the primary's filesystem is the unusual case, and it should have to say so. No
such key today; add one when something needs it.

## Dependencies

- **0096**, which fixed the double-graft half of the argv contract and added the
  absolute-ENTRYPOINT coverage in `crates/outrig/tests/library_surface.rs` that this task's
  changes have to keep passing.

## See also

- `crates/outrig/src/container/enter/launcher.rs` -- `main`, and the argv contract in the header.
- `crates/outrig/src/container/sidecar.rs` -- `build_primary_view_argv`, `entrypoint_create_args`.
- `plan/done/0089-outrig-enter-helper.md`, `plan/done/0090-primary-view-sidecars.md`,
  `plan/done/0091-host-side-user-bootstrap.md` -- the bootstrap this task brings entrypoint
  hosts into line with.
- `plan/todo/0103-primary-view-relative-entrypoint.md` -- the other launcher change, queued
  next; both edit `main()`.
