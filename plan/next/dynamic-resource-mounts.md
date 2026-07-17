# Dynamic resource mounts via host-side FUSE

## Context

Today `[[workspace.mounts]]` (see `doc/concepts/workspace.md`) lets a session declare extra
host directories that the container sees at fixed container paths. Those mounts are established
at `podman run` time and are static for the life of the session: podman fixes a container's
mount set at creation, and there is no `podman mount --add` for a running container. Adding a
new resource tree mid-session today means recreating the container.

This spec covers making the *contents* of a resource mount dynamic without recreating the
container -- so an OutRig supervisor (or a user, or the agent itself) can add, reshape, or
revoke resources visible under a mount point while the session runs.

Three use cases motivate the feature, all read-only:

- **Supply material.** Stage docs, datasets, or reference trees the agent reads, adjusting what
  is present as the task evolves -- without a container restart.
- **Hand files to a live agent.** A human operator drops or removes files under the mount
  mid-task through a CLI (`outrig resource ...`).
- **Agent-facing on-demand.** The agent requests a resource through an MCP tool and it appears
  under the mount, subject to host-side policy (see fork 3).

Read-only is the committed default and covers all three; read-write dynamic mounts are a
separate, larger question left open (fork 6).

The mechanism is a host-side userspace filesystem (FUSE): mount it once on the host, bind it
into the container at creation like any other `[[workspace.mounts]]` entry, and thereafter
change what it serves from the host side. The container is oblivious -- it sees an ordinary
directory and issues ordinary syscalls; all dynamism and policy live on the trusted host side,
which preserves the "podman is the source of truth, sandbox stays unprivileged" invariant that
the rest of the design leans on (`doc/concepts/containers.md`, `doc/concepts/workspace.md`).

This is deliberately post-v0: it introduces a new host-side long-lived subsystem (a mount
manager owning FUSE daemon processes) with its own liveness and teardown failure surface, on
top of the existing static-mount path. It is close kin to the network interceptor -- the other
per-session host-side subsystem -- and should follow that subsystem's construction, liveness,
and cleanup shape.

## Goal

Let a session expose one or more resource mount points whose contents can change while the
container runs, backed by a host-side FUSE filesystem that OutRig's supervisor owns, monitors,
and tears down -- with the container needing no knowledge of the backing mechanism.

## Deliverables

- A config surface for declaring dynamic resource mounts: a distinct `[[workspace.resources]]`
  block (fork 2), parallel to `[[workspace.mounts]]`.
- A supervisor-owned **resource-mount manager** that, per session, tracks
  `(sources -> host mountpoint -> container bind)` triples, starts the FUSE daemon, mounts it
  on the host before the container's `podman run`, and cleans up on teardown.
- The container-side bind wiring: the host FUSE mountpoint bind-mounted at the declared
  container path, read-only by default, using the same validation as existing extra mounts
  (absolute, non-`/`, no duplicate container paths).
- A **daemon liveness sensor**: a dead FUSE daemon emits no podman event, so detection cannot
  reuse the podman-events watcher. Watch the daemon PID (waitpid / child-exit) or probe the
  host mountpoint (statfs surfaces a sticky `ENOTCONN`), and feed the existing session-death
  diagnostic path so a dead daemon fails the session loudly rather than silently.
- **Host-side stray identification and `outrig clean` integration.** `outrig clean` and the
  panic-hook sweep both reap *podman* objects (by name / `org.outrig.session` label); neither
  unmounts a host FUSE mount today. Either add a session-scoped marker the sweep can scan so a
  mount orphaned by a hard-killed supervisor is found and unmounted, or accept `Drop`-only
  defense as the network interceptor does -- but decide explicitly, because the naive reading of
  "integrate with the three cleanup layers" is not free here.
- Ordered teardown: unmount the FUSE filesystem cleanly on session end so mounts and dead
  endpoints do not leak; slot into the existing teardown after the containers stop.
- A host-side API and command for mutating a live mount's contents (add / reshape / revoke),
  scoped per session, across the three surfaces in fork 3.
- An **audit sink**, `<session_dir>/logs/resources.jsonl`, recording every add / remove /
  revoke with its requester (config / CLI / agent), source, and outcome -- mirroring the
  network interceptor's `network.jsonl`.
- Documentation: a concepts page (or a section in `workspace.md`) covering the model, read-only
  default, UID mapping behavior, liveness / restart semantics, and cleanup. Carry a leading
  `> **TODO: Incomplete** -- ...` blockquote until the surface lands.

## Config sketch

A dynamic resource mount declares a name, a container path, and an optional set of initial
sources. Unlike a static mount it has no user-supplied `host-path`: the manager synthesizes the
host mountpoint under the session directory.

```toml
[[workspace.resources]]
name           = "context"             # host mountpoint: <session_dir>/resources/<name>
container-path = "/resources/context"  # absolute, non-"/", no duplicate targets
sources        = ["../briefs"]         # optional initial served trees; may change at runtime
# read-only for now; read-write is deferred (fork 6)
```

Container-path validation reuses the existing extra-mount rules; `access` is fixed read-only for
now.

## Runtime Behavior

At session start the resource-mount manager mounts the chosen FUSE filesystem on the host under
the session directory (`<session_dir>/resources/<name>`), then OutRig adds a bind of that host
path into the container at the declared container path during the normal `podman run` mount
assembly -- alongside the primary workspace and static extra mounts. A resource declared for
more than one container (the primary plus opting-in sidecars) is one daemon bound in several
times, never one daemon per container (fork 7).

Because the bind captures a filesystem that is already mounted, a single custom daemon serving a
synthesized tree needs no mount propagation at all: adding or removing a served file, or a
served plain subdirectory, is a change in the daemon's readdir / getattr replies, not a new
kernel mount, and appears live in the container. `rshared` propagation matters only under a
*union* backing that introduces a genuinely new *mount* (a real branch) under the shared path
after the container is running -- one more reason to prefer the single-daemon design (fork 1).

The container sees an ordinary read-only directory. It cannot tell the mount is FUSE-backed and
needs no client, library, or config. Mutating what appears there is a host-side operation
against the manager, never a podman operation and never a container-side action.

Ownership of served files is subject to podman's rootless UID remapping (`--userns=keep-id`), so
the manager pins a presentation policy (typically read-only, host-UID-owned) rather than leaving
it to chance. See Risks for the `allow_other` interaction this raises.

**Revocation.** Removing a source mid-session drops it from the container's view. The policy for
a file the container still holds open at revoke time -- keep serving the open handle until close,
or start returning errors -- is a design call that differs between a union backend and a custom
daemon; the custom daemon can choose either explicitly.

**Liveness.** If the FUSE daemon dies, the container's mountpoint returns sticky `ENOTCONN`
until remounted. The manager detects this through the liveness sensor above and, by default,
fails the session with a clear diagnostic (the same typed session-death path the primary-death
watcher uses), never leaving a silent broken mount (fork 5).

**Teardown ordering.** On session end the manager unmounts the host FUSE mount *after* every
container is stopped (so nothing is using the bind) and *before* the session record is
finalized, then removes `<session_dir>/resources/<name>`. The manager is dropped after the
container handles, so the `Drop`-path order mirrors the explicit-teardown order -- the same
field-order discipline the session runtime already uses for its network interceptor and
containers.

## Acceptance

- A session can declare a dynamic resource mount; the container sees it as a normal directory at
  the declared path, read-only by default.
- Adding a resource on the host mid-session appears in the container without recreating the
  container and without a container-side action.
- Revoking a resource on the host removes it from the container's view mid-session.
- A rootless `--userns=keep-id` container can traverse the host FUSE bind and read a served file
  -- the `allow_other` / `user_allow_other` interaction is resolved (demonstrated by a
  prototype; see Risks).
- Killing the FUSE daemon produces a clear session-level diagnostic (or an automatic remount),
  not a silent sticky-`ENOTCONN` mount.
- Session teardown unmounts the FUSE filesystem; no orphaned mounts or dead endpoints remain,
  and a mount stranded by a hard-killed supervisor is reclaimed by the chosen stray mechanism
  (new host-side sweep, or `Drop`-only -- whichever the manager commits to).
- Every resource mutation (config, CLI, or agent) is recorded in
  `<session_dir>/logs/resources.jsonl`.
- An agent-facing add outside the operator-declared allowlist is refused and audited.
- Static `[[workspace.mounts]]` behavior is unchanged when no dynamic mount is declared.
- Served files present with sane, policy-pinned ownership under `--userns=keep-id`.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean a
prototype should confirm), or **Open** (deferred).

1. **Backing implementation -- Recommended (confirm with a spike).** Off-the-shelf union
   (`mergerfs`, or `fuse-overlayfs`, which podman already depends on) versus a custom Rust
   `fuser` / `fuse3` daemon. The in-scope requirements -- agent-facing on-demand mounting,
   per-resource revoke, and per-mutation audit -- want per-request policy hooks that a union
   filesystem does not offer, so the custom read-only daemon is the target. An off-the-shelf
   union is still useful as a throwaway early spike behind the same container-facing contract,
   to de-risk the FUSE-under-rootless questions before any daemon code is written.

2. **Config shape -- Resolved: a distinct `[[workspace.resources]]` block**, not a
   `dynamic = true` attribute on `[[workspace.mounts]]`. A dynamic mount has no user-supplied
   host path -- the manager synthesizes it under the session directory -- and it declares a
   `name` plus an evolving `sources` set, so its fields genuinely differ from a static mount.
   The container-path validation (absolute, non-`/`, no duplicate targets) and the read-only
   default are reused from the static path.

3. **Who may mutate a live mount -- Resolved: three surfaces in scope; policy schema open.**
   Config seeds the initial `sources`; an `outrig resource {add,rm,ls}` CLI subcommand covers
   the human-hands-files case; an MCP tool covers agent-facing on-demand. The agent-facing
   surface is the widest trust boundary and is constrained accordingly: adds are restricted to
   operator-declared allowlisted host roots, presented read-only, and every mutation is audited.
   The agent never issues a podman command -- it calls the trusted host-side manager, which
   keeps the "sandbox stays unprivileged" invariant intact. Open: the exact allowlist / policy
   schema and how it is declared in config.

4. **FUSE placement -- Resolved: host-side daemon + bind-in (Strategy A).** This keeps
   `/dev/fuse` and `SYS_ADMIN` out of the sandbox. Strategy B (in-container daemon, needing
   `--device /dev/fuse` and elevated caps) erodes the sandbox and is rejected / out of scope;
   recorded here only as the rejected fork.

5. **Liveness policy on daemon death -- Resolved default; richer modes open.** Default: fail the
   session with a clear diagnostic, reusing the typed session-death path. Auto-remount-and-
   continue and degrade-to-static-snapshot are deferred; they interact with how hard the agent
   depends on the resources being present.

6. **Read-write dynamic mounts -- Open.** Read-only is the safe default and covers all three
   in-scope use cases. Whether to support read-write dynamic mounts at all -- and how writes
   flow back through a union to a specific branch -- is a separate, larger question.

7. **Sidecars -- Resolved: one daemon, many binds.** A resource is served by a single FUSE
   daemon whose host mountpoint is bound into every container that declares it -- the primary by
   default, and any sidecar that opts in through the same mount-list mechanism sidecars already
   use for their extra mounts. The manager does not run one daemon per container.

8. **VM-era backend -- Open (forward-compat).** If OutRig ever grows a microVM isolation
   backend, `virtiofs` is the native mechanism for live-shared, dynamically-changing host
   directories into a guest. Keep the container-facing contract (a plain read-only directory at
   a fixed path) backend-agnostic so a virtiofs backing can slot in later.

## Risks and prototype spikes

- **FUSE `allow_other` under rootless `--userns=keep-id`.** A container running in its own user
  namespace may be unable to traverse a host FUSE bind unless the daemon mounts with
  `allow_other`, which itself requires `user_allow_other` in `/etc/fuse.conf`. This is the
  single biggest unknown; de-risk it with a spike (an off-the-shelf union bound into a real
  session container) before committing to the approach. If host `/etc/fuse.conf` edits turn out
  to be required, that is a documented prerequisite and a portability cost.
- **Host-side stray sweep.** No host-filesystem / mount sweep exists in `outrig clean` or the
  panic-hook layer today; decide new-sweep versus `Drop`-only (see Deliverables).
- **Liveness sensor is net-new.** podman's event / wait / inspect machinery does not observe a
  FUSE daemon's death; the sensor is a new signal source, not a reuse of the existing watcher.
- **Revoke-with-open-fd.** Define the semantics for a container fd still open against a revoked
  resource before choosing the backend, since union and custom daemons behave differently.

## Dependencies

- **Soft: the static extra-mount path** (`[[workspace.mounts]]` assembly in the `podman run`
  path). This feature extends that mount-assembly and validation surface.
- **Soft: the cleanup layers** (explicit stop / Drop / panic-hook sweep) and `outrig clean`,
  which the mount manager's teardown must integrate with -- and which, for host mounts, it must
  extend rather than merely reuse (see Deliverables).
- **Soft: the network interceptor**, the existing per-session host-side subsystem whose
  construction, liveness, teardown, and `Drop` shape are the structural template here.

## See also

- `doc/concepts/workspace.md` -- static `[[workspace.mounts]]`, read-only default, UID mapping.
- `doc/concepts/containers.md` -- what OutRig sets in the run; sidecar defaults and cleanup.
- `plan/next/network-interceptor-mitm.md` -- another host-side, session-scoped subsystem with a
  similar "trusted host side owns the mechanism" shape.
