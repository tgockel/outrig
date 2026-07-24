# 0091 -- Bootstrap the container user from the host, without `useradd`

## Context

`Container::bootstrap_user` (`crates/outrig/src/container/mod.rs:384-422`) materializes an
in-container user and group matching the host UID/GID, because `--userns=keep-id` maps them
1:1 and every MCP server and tool then runs as `--user=<uid>:<gid>`. It does that with a chain of
`podman exec --user=0:0` round-trips: `getent group`, maybe `groupadd`, `getent passwd`, maybe
`useradd`, `mkdir -p /home/<user>`, `chown`.

That makes the *image* responsible for tools OutRig needs. `doc/concepts/containers.md` tells
users so directly -- don't set up a user in the Dockerfile, but do ship `useradd`/`groupadd` --
and all four generated templates pay for it:
`crates/outrig-cli/src/image_setup/templates/header.debian.dockerfile` installs `passwd`,
`header.alpine.dockerfile` installs `shadow`. `getent` is a third unstated requirement. A user
bringing a minimal or distroless base image gets a startup failure from a step they did not know
existed, for a user they never asked for.

The prototype at <https://github.com/tgockel/prototype-podman-shared-fs> shows the way out. A
host process can join the container's mount namespace and nothing else, and then simply write
the files. Its check #5 is the relevant one: a host process writing into the container's
filesystem lands as uid/gid 1000 on both sides, with no `chown`.

This is the same technique as 0089/0090 pointed the other way -- outside-in rather than
sidecar-in -- and it needs no helper binary, no new capabilities, and no config surface.

### What this consumes

`plan/next/sidecar-primary-bootstrap-overlap.md` proposed reclaiming the time this chain costs.
Its own summary: 0086 made sidecar bring-up concurrent across sidecars but left Phase-A
image-ensure starting only *after* the primary is started and `bootstrap_user`'d, though the two
are independent. It scoped its own win as bounded -- "`bootstrap_user` is a short chain of
`podman exec` round-trips (~100ms), so overlapping it with sidecar ensure saves at most ~that,
and only for sessions that declare sidecars" -- against a cost it described as "restructuring the
primary path's delicate abort tail (stop primary + finalize row + return on bootstrap failure)".

That trade was already marginal. Once bootstrap is a single forked child doing direct file
writes, the ~100ms it was trying to hide is gone, and there is nothing left to overlap. The entry
is obsoleted rather than deferred, and this task deletes it. If this task is abandoned, recover
it from git history rather than reconstructing it.

## Goal

Bootstrap the in-container user and group by writing `/etc/group`, `/etc/passwd`, and
`/home/<user>` directly from the host, inside the container's mount namespace -- so any image
works, whether or not it ships `shadow`, `passwd`, or `getent`.

## Deliverables

- A namespace-entry primitive in `crates/outrig/src/container/`: given a container, run a closure
  inside its mount namespace in a forked child and return the child's status. The PID lookup
  already exists as `container_pid` (`crates/outrig/src/network.rs:909`), private to `network`;
  promote it rather than writing a second one. 0090 needs the same lookup.
- `Container::bootstrap_user` reimplemented on top of it, preserving its current contract
  exactly: probe first, create only if absent, retry name collisions with `_` up to
  `BOOTSTRAP_RETRIES`, and record `user_name` / `group_name` on the struct for
  `Container::exec_stdio`.
- The `podman exec` chain retained as a fallback when the pause process or `setns` is
  unavailable, so a non-rootless or otherwise unusual podman keeps working.
- `passwd` and `shadow` dropped from the four generated Dockerfile templates.
- Docs in the same commit: `doc/concepts/containers.md` (the "ship `useradd`/`groupadd`"
  requirement goes away, and "What outrig sets in the run" gains the new step),
  `doc/usage/run.md`'s ordered startup sequence, and `doc/reference/config.md` if it repeats the
  image requirement.
- `plan/next/sidecar-primary-bootstrap-overlap.md` deleted.

## Runtime behavior

Ordering is forced, and the wrong order fails as EPERM rather than as anything informative:

1. `setns(pidfd_open(<pause>), CLONE_NEWUSER)` -- join the **rootless** user namespace first.
   `<pause>` is the rootless pause process at `$XDG_RUNTIME_DIR/libpod/tmp/pause.pid`, which owns
   the user namespace every rootless container is created inside. If the file is absent, `podman
   info` materializes it.
2. `setns(pidfd_open(<container>), CLONE_NEWNS)` -- join the container's mount namespace. Doing
   this first is EPERM.

Join the **rootless** namespace, not the container's. This is a correctness requirement here, not
a preference as it is in the prototype: in the container's own user namespace the process is
uid 1000 with no capabilities and cannot write a root-owned `/etc/passwd`. In the rootless
namespace it is uid 0 with full capabilities *over that namespace*, which covers every UID mapped
into it, including the container's root.

Three consequences to get right:

- **`setns(CLONE_NEWUSER)` requires a single-threaded process.** Tokio is running by the time
  bootstrap happens, so this must be a `fork()` whose child does the `setns` and `_exit`s, or a
  `Command::pre_exec`. The child must not touch the async runtime, allocate through anything the
  parent's threads hold a lock on, or run destructors -- keep it to syscalls and `_exit`.
- **Ownership.** `/etc/passwd` and `/etc/group` are owned by the container's root. Under keep-id
  the rootless namespace's uid 0 *is* the container's uid 1000, so writing a new file and
  renaming it over the old one would silently reassign ownership to the session user. Append in
  place, with the fd opened `O_WRONLY | O_APPEND`, and handle a file with no trailing newline.
- **`/home/<user>` needs no `chown`.** Created from the rootless namespace it already lands owned
  by the host uid/gid, which is what the current `chown` step is for. Drop the step rather than
  reimplementing it.

Keep the existing probe-first structure. `plan/done/0009-runtime-user-bootstrap.md` records that
modern podman auto-injects the host UID/GID into `/etc/passwd` and `/etc/group` under keep-id, so
on podman 5.x both entries frequently exist already and the write is skipped entirely. Reading
the files directly makes that check cheaper and removes the `getent` dependency at the same time.

Sidecars use the same path. `bootstrap_needed`
(`crates/outrig/src/container/sidecar.rs:132-138`) already decides which sidecars want a
bootstrap; only the target container name changes.

## Validation

Nothing new is user-declarable, so there is no config validation. The failure surface is
runtime, and each mode needs a distinguishable message:

| Failure                                  | Behavior                                      |
|------------------------------------------|-----------------------------------------------|
| pause process missing and unrecoverable  | fall back to the `podman exec` chain          |
| `setns` returns EPERM                    | fall back, with the reason in the transcript  |
| container not running (`State.Pid = 0`)  | hard error -- there is no namespace to join   |
| `/etc/passwd` unwritable in-namespace    | hard error naming the file, not a silent skip |

## Acceptance

- A session launches against an image with no `shadow`, no `passwd`, and no `getent` -- an
  unadorned `FROM docker.io/library/alpine` is enough -- and an MCP server started in it runs as
  the host uid with a working `HOME`.
- On an image that *does* ship them, the resulting `/etc/passwd` and `/etc/group` are
  byte-identical to what the `useradd` path produced, and retain their original owner and mode.
- The existing bootstrap tests pass unchanged, including the collision-retry behavior and the
  podman-5.x probe-or-plant case from `plan/done/0009`.
- Forcing the fallback (unset `XDG_RUNTIME_DIR`, or point it at an empty directory) still
  bootstraps successfully via `podman exec`.
- The four generated templates no longer install `passwd`/`shadow`, and
  `crates/outrig-cli/tests/embedded_image.rs` reflects the new template text.
- Startup does five fewer `podman exec` round-trips, visible as fewer podman lines in the session
  transcript.

## Open questions

- **How long to keep the fallback.** It is cheap to keep and removes all regression risk for
  exotic podman setups, but it is a second implementation of the same contract. Keep it for one
  release, then decide with real reports rather than now.
- **Whether the primitive should also handle exec.** 0089's launcher does `execveat` into a
  payload; this task only needs file writes in a closure. Keep the primitive at the closure
  level and let 0089's binary stay separate -- they run in different places and share no code
  path, only a technique.
- **`/etc/shadow`.** The current `useradd` writes one; the direct path would not. Nothing in
  OutRig authenticates as this user, so the entry has no reader. Note it and skip it, but check
  whether any base image's `su`/`sudo` path cares before deleting the possibility.

## Dependencies

None. Independent of 0088-0090, which run the technique in the other direction.

## See also

- <https://github.com/tgockel/prototype-podman-shared-fs> -- `enterfs.py` is the host-side
  reference; the README's "Ordering is forced" section is the part that matters here.
- `plan/done/0009-runtime-user-bootstrap.md` -- the contract this preserves, and the podman-5.x
  auto-injection hazard.
- `doc/concepts/containers.md` -- the image requirements this removes.
