# 0091 -- Bootstrap the container user from the host, without `useradd`

## Context

`Container::bootstrap_user` (`crates/outrig/src/container/mod.rs:442-490`) materializes an
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
(It was already deleted in `8560c3a0`, when this task was written; recover it from git history if
this task is abandoned.)
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

- A namespace-entry primitive in `crates/outrig/src/container/`: given a container, run a fixed
  step sequence inside its mount namespace in a forked child and hand the result back. The PID
  lookup already exists -- 0090 promoted it to `Container::pid` (`mod.rs:382`) -- and the
  fork / `setns` / `SCM_RIGHTS` plumbing already exists inside `network.rs`; promote both rather
  than writing a second copy.
- `Container::bootstrap_user` reimplemented on top of it, preserving its current contract
  exactly: probe first, create only if absent, retry name collisions with `_` up to
  `BOOTSTRAP_RETRIES`, and record `user_name` / `group_name` on the struct for
  `Container::exec_stdio`.
- The `podman exec` chain retained as a fallback when namespace entry is unavailable, so a remote
  or otherwise unusual podman keeps working, plus an `OUTRIG_BOOTSTRAP` escape hatch
  (`auto` / `direct` / `exec`) to select the path by hand.
- `passwd` and `shadow` dropped from the generated Dockerfile templates, from the worked examples
  in the AI design prompt, and from this repo's own `outrig-standard` image.
- The `user_bootstrap_package_missing` Dockerfile warning made conditional: emitted only when the
  host would fall back to the `podman exec` chain, and repeated at run time when the fallback
  actually fires.
- Docs in the same commit: `doc/concepts/containers.md` (the "ship `useradd`/`groupadd`"
  requirement goes away, and "What outrig sets in the run" gains the new step),
  `doc/concepts/workspace.md`'s bootstrap pseudocode, `doc/usage/run.md`'s ordered startup
  sequence, `doc/usage/image.md`, `doc/quickstart.md`, and the byte-identical copies of those
  pages under `crates/outrig-cli/src/mcp_self/docs/`.

## Runtime behavior

Ordering is forced, and the wrong order fails as EPERM rather than as anything informative:

1. `setns(/proc/<container>/ns/user, CLONE_NEWUSER)` -- join the container's user namespace first.
   Joining a mount namespace straight from the host is EPERM.
2. `setgid(0)` / `setuid(0)` -- become the container's root.
3. `setns(/proc/<container>/ns/mnt, CLONE_NEWNS)` -- join the container's mount namespace.

Join the **container's own** user namespace. The prototype joins the rootless pause process's
namespace instead (`$XDG_RUNTIME_DIR/libpod/tmp/pause.pid`), and both work -- `setns(CLONE_NEWUSER)`
grants `CAP_FULL_SET` in the namespace it joins regardless of uid, and a capability held in a
namespace is held in all its descendants -- but the container's own namespace is reached without
discovering, validating, or materializing a pause process. It is also the route `network.rs`
already runs against these same containers on every intercepted session, which is why the
`setuid(0)` there works. A pause process that died and was regenerated owns a *sibling* namespace,
not an ancestor of the container's, so the pause route needs a staleness cross-check that this one
does not. Rootful podman has no separate user namespace to join: step 1 returns EINVAL, which
means "already there" and is not a failure.

Four consequences to get right:

- **`setns(CLONE_NEWUSER)` requires a single-threaded process.** Tokio is running by the time
  bootstrap happens, so this must be a `fork()` whose child does the `setns` and `_exit`s. The
  child must not touch the async runtime, allocate through anything the parent's threads hold a
  lock on, or run destructors -- keep it to syscalls and `_exit`. That rules out parsing in the
  child, so the child opens `/etc/passwd` and `/etc/group` and passes the fds back over
  `SCM_RIGHTS`; the parent reads and appends through them with ordinary code, since the kernel
  checks permission at `open`, not at `write`.
- **Ownership.** `/etc/passwd` and `/etc/group` are owned by the container's root, and writing a
  new file and renaming it over the old one would reassign ownership and mode. Append in place,
  with the fd opened `O_APPEND`, and handle a file with no trailing newline.
- **`/home/<user>` still needs its `chown`.** Created inside the container's user namespace it
  lands owned by the container's root, so the existing `chown` step stays -- as `chown(uid, gid)`
  in a second forked child rather than a `podman exec`.
- **Names go into a colon-delimited file unchecked.** `useradd` rejected a host user name
  containing `:` or a newline; a direct write would corrupt the file. Sanitize before writing.

Keep the existing probe-first structure. `plan/done/0009-runtime-user-bootstrap.md` records that
modern podman auto-injects the host UID/GID into `/etc/passwd` and `/etc/group` under keep-id, so
on podman 5.x both entries frequently exist already and the write is skipped entirely. Reading
the files directly makes that check cheaper and removes the `getent` dependency at the same time.

Sidecars use the same path. `bootstrap_needed`
(`crates/outrig/src/container/sidecar.rs:219-231`) already decides which sidecars want a
bootstrap; only the target container name changes.

## Validation

Nothing new is user-declarable, so there is no config validation. The failure surface is
runtime, and each mode needs a distinguishable message:

| Failure                                  | Behavior                                      |
|------------------------------------------|-----------------------------------------------|
| `/proc/<pid>/ns/*` unopenable, `fork`    | fall back to the `podman exec` chain          |
| `setns` returns EPERM                    | fall back, with the reason in the transcript  |
| container not running (`State.Pid = 0`)  | hard error -- there is no namespace to join   |
| `/etc/passwd` unwritable in-namespace    | hard error naming the file, not a silent skip |

The dividing line is whether the container was touched: everything up to and including the mount
namespace join leaves it untouched, so falling back is safe; anything past it is fatal, because a
half-applied direct bootstrap re-run over `podman exec` would report a misleading error.

## Acceptance

- A session launches against an image with no `shadow`, no `passwd`, and no `getent` -- an
  unadorned `FROM docker.io/library/alpine` is enough -- and an MCP server started in it runs as
  the host uid with a working `HOME`.
- On an image that *does* ship them, the entries the direct path writes are field-equivalent in
  name / uid / gid / home to what `useradd` produced, and `/etc/passwd` and `/etc/group` retain
  their original owner and mode. The shell field is the one that cannot match byte-for-byte
  across distros -- Debian's `/etc/default/useradd` says `/bin/sh`, upstream shadow's built-in
  default is `/bin/bash`, which Alpine does not ship -- so `/bin/sh` is the canonical value and
  the line is asserted against `name:x:uid:gid::/home/name:/bin/sh`.
- The existing bootstrap tests pass unchanged, including the podman-5.x probe-or-plant case from
  `plan/done/0009`. The collision retry, which had no test before, gains one: it is now a pure
  scan over the parsed file rather than a loop over `useradd` exit codes.
- Forcing the fallback with `OUTRIG_BOOTSTRAP=exec` still bootstraps successfully via `podman
  exec`. (The pause process is not part of this route, so `XDG_RUNTIME_DIR` is no longer the
  lever.)
- The generated templates no longer install `passwd`/`shadow`, and
  `crates/outrig-cli/tests/image_add_render.rs` asserts they stay out.
- Startup does five fewer `podman exec` round-trips, visible as fewer podman lines in the session
  transcript.

## Open questions

- **How long to keep the fallback.** It is cheap to keep and removes all regression risk for
  exotic podman setups, but it is a second implementation of the same contract. Keep it for one
  release, then decide with real reports rather than now.
- **Whether the primitive should also handle exec.** 0089's launcher does `execveat` into a
  payload; this task only needs file descriptors handed back out. Keep the primitive at the
  fixed-step level and let 0089's binary stay separate -- they run in different places and share
  no code path, only a technique.
- **`/etc/shadow`.** The current `useradd` writes one; the direct path does not. Nothing in
  OutRig authenticates as this user, `getpwnam` never consults shadow, and the entry `useradd`
  writes is a locked (`!`) password -- so `su` and `sudo` fail to authenticate identically with
  or without it. Skipped, and the reasoning recorded where the write would go.

## Dependencies

None. Independent of 0088-0090, which run the technique in the other direction.

## See also

- <https://github.com/tgockel/prototype-podman-shared-fs> -- `enterfs.py` is the host-side
  reference; the README's "Ordering is forced" section is the part that matters here.
- `plan/done/0009-runtime-user-bootstrap.md` -- the contract this preserves, and the podman-5.x
  auto-injection hazard.
- `doc/concepts/containers.md` -- the image requirements this removes.

## Decisions

- **Joined the container's own user namespace, not the rootless pause process's.** The task file
  originally mandated the pause route and justified it as a correctness requirement: "in the
  container's own user namespace the process is uid 1000 with no capabilities and cannot write a
  root-owned `/etc/passwd`". That is wrong. `setns(CLONE_NEWUSER)` grants `CAP_FULL_SET` in the
  namespace it joins, regardless of the caller's uid -- which is precisely why the `setuid(0)` at
  `network.rs:1003` has worked for every intercepted session since 0060. Joining the container's
  namespace and then becoming its root reaches the same place with no pause-pid discovery, no
  `XDG_RUNTIME_DIR` dependency, no `podman info` to materialize a missing pause process, and no
  staleness hazard (a regenerated pause process owns a *sibling* namespace of the container's,
  where the mount join then fails EPERM -- or worse, the pid has been recycled). Route P remains
  implementable if a real report ever needs it: the child takes its namespace fds as parameters,
  so it would be a different pair of fds, not different logic.
- **File descriptors handed back over `SCM_RIGHTS`, rather than file contents.** The child cannot
  parse -- `fork()` in a live tokio process gives it every lock the other threads held, so an
  allocation can deadlock. Passing the two open descriptors back means the parent reads and
  appends through ordinary `File` code: the kernel checks permission at `open`, so the
  unprivileged parent inherits the child's access without inheriting its constraints. The
  alternative (child copies bytes through a pipe, parent parses, second child writes back
  precomputed blobs) needs a framing protocol and one more fork for the same result.
- **The fork/`setns`/`SCM_RIGHTS` plumbing was promoted out of `network.rs` into `nsfork.rs`
  rather than copied.** Two changes came with the move: `send_fds` no longer allocates (it built
  a `Vec` *inside the forked child*, the exact deadlock hazard above) and its control buffer is
  now 8-aligned rather than 1-aligned, which is what `msg_control` actually requires. The 1-byte
  dummy payload became an 8-byte `[step, errno]` status, which is how the bootstrap child reports
  where it stopped.
- **`NsStep::is_entry()` decides fallback, not the error kind.** Everything up to and including
  the mount-namespace join leaves the container untouched, so the `podman exec` chain can still
  run; anything past it has already appended to `/etc/passwd`, and re-running the whole chain
  would report a misleading error. The decision is taken once, on the descriptor-opening pass.
- **`OUTRIG_BOOTSTRAP=auto|direct|exec` replaces the `XDG_RUNTIME_DIR` lever.** With the pause
  process out of the picture, nothing in the environment forces the fallback, so the e2e test had
  no way to exercise it. The knob doubles as the workaround to hand anyone who reports an exotic
  podman during the keep-the-fallback window, and `direct` proves a host isn't silently
  downgrading. It lives in its own test binary (`runtime_user_fallback.rs`) because the mode is
  read once per process.
- **The `user_bootstrap_package_missing` warning became conditional on both sides.** The static
  Dockerfile lint now takes a `UserBootstrap` and only fires when this host would fall back
  (probed once via `podman info --format {{.Host.ServiceIsRemote}}`, with an unanswerable probe
  counting as "would fall back"); the runtime path says the same thing again, with the real
  reason, when the fallback actually fires. Consequence worth knowing: on a remote-podman host,
  the templates OutRig itself generates now trip that warning, because they no longer install
  `passwd`/`shadow`. That is accurate advice for that host rather than a bug.
- **The fallback's exit code 127 is fatal, not a collision.** The old loop treated every non-zero
  exit as a name collision and burned all ten retries when the image simply had no `useradd`.
  It now stops on the first 127 and names the package to install.
- **Host names are sanitized before they reach the file.** `useradd` used to reject a name
  containing `:` or a newline (after ten `_`-suffixed attempts); a direct write would corrupt
  `/etc/passwd` instead. Non-portable characters become `_`, and a name with nothing usable left
  falls back to `u<uid>` / `g<gid>`. Strictly better than the old behavior, and the reason
  `sanitize_name` is not merely cosmetic.
- **Acceptance's "byte-identical to what `useradd` produced" was weakened to field-equivalence.**
  The shell field cannot match across distros -- Debian's `/etc/default/useradd` says `/bin/sh`,
  upstream shadow compiles in `/bin/bash`, which Alpine does not ship -- so `useradd`'s own output
  was never identical either. `/bin/sh` is the canonical value; the ownership and mode half of the
  criterion does hold, and is tested.
- **`Container::pid` caches its answer.** Bootstrap made it the third caller during startup, after
  the network interceptor and `view = "primary"` sidecars, each paying a ~20-40ms `podman inspect`
  for a value that cannot change: a running container keeps its init PID until it stops, and
  nothing restarts a stopped one. The cache only ever holds a success, since `pid()` errors on
  `State.Pid == 0`.
- **The Dockerfile validator's host probe happens once per MCP server, not once per tool call.**
  `podman info` costs ~1.1s on a warm host. `SelfServer` resolves it at `serve_stdio` and carries
  the answer, so `list_docs` doesn't pay for advice only `validate_dockerfile` gives.
- **Existing e2e fixtures keep their `shadow` installs.** Only `bootstrap_reuses_existing_entry`
  still needs it (it plants a conflicting entry with `useradd` before bootstrapping). Stripping
  the rest would have touched a dozen files for no coverage gain; the new bare-alpine tests carry
  the acceptance.
