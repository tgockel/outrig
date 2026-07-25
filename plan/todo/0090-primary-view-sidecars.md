# 0090 -- Sidecars that share the primary container's filesystem view

## Context

There are two ways to place an MCP server today, and each charges the user for it.

**In the primary**, over `podman exec -i` (`build_exec_argv`,
`crates/outrig/src/container/mod.rs:506-525`). The server binary has to exist in the user's
image. `crates/outrig/tests/fixtures/mcp-fs/Dockerfile` shows the going rate -- nodejs, npm, and
a global npm install, so that one filesystem server can run. Every image needs every tool, and
the user maintains that.

**In a sidecar**, with `workspace = "ro"` or `"rw"` (`SidecarWorkspaceAccess`,
`crates/outrig/src/config/mod.rs:909-925`). OutRig re-binds the same *host* path into a second
container at the same container path. The image stays untouched, but the server does not see
what the agent sees:

- no primary rootfs -- no toolchain, no `/etc`, none of the image's own contents;
- the host's view of the workspace, not the primary's, so anything mounted *inside* the
  workspace is missing;
- nothing else the primary has -- no `[[workspace.mounts]]` resource trees unless separately
  declared, no tmpfs, no interceptor state.

An indexer, a language server, or a filesystem MCP server wants the third thing neither offers:
the tool in its own image, looking at the primary's actual filesystem, at the primary's paths.

The prototype at <https://github.com/tgockel/prototype-podman-shared-fs> demonstrates it working
against stock upstream images, and its check #12 is precisely the shape OutRig wants -- an
unmodified `docker.io/mcp/filesystem` (Alpine/musl, node 22) serving a Debian/glibc target's
project over stdio, with neither image knowing about the other.

## Goal

A sidecar that runs a third-party MCP image against the primary container's filesystem view:
identical paths, the primary's rootfs and every mount, with the tool supplying its own runtime
and the primary image untouched.

Scope this task to **entrypoint-stdio sidecars** -- the `image =` form, where the container's
lifetime is the server's lifetime. Whether `podman exec` lands in the grafted namespace or the
sidecar's original one depends on how the OCI runtime resolves the init process's namespaces,
and that is unverified. Exec-stdio hosts stay on today's bind-mount path and get a validation
error until someone measures it.

## Deliverables

- A `view` key on `[images.<x>.sidecars.<sc>]` and on the inline anonymous form, carried through
  `SidecarPlan` (`crates/outrig/src/container/sidecar.rs:53-66`) and `SidecarSpec`.
- Launch flags emitted from `append_launch_flags` (`crates/outrig/src/container/mod.rs:688-719`),
  so `podman create` picks them up without separate plumbing -- the same place 0087 put
  `devices` and `no-new-privileges`, and for the same reason.
- The primary's PID resolved by reusing `container_pid` (`crates/outrig/src/network.rs:909`),
  which already runs `podman inspect --format {{.State.Pid}}` to drive the interceptor's
  `nsenter`. It is private to `network`; promote it rather than writing a second one.
- The sidecar image's ENTRYPOINT and CMD read via a new sibling of `read_image_labels`
  (`crates/outrig/src/image.rs:528-549`), reusing its `--format {{json ...}}` shape.
- Teardown wired into the existing `podman events --filter event=died` watcher
  (`crates/outrig-cli/src/cli/watcher.rs:146`): the primary dying tears down its `view =
  "primary"` sidecars.
- Validation, config schema, and `show-merged` serialization.
- Docs in the same commit: `doc/concepts/mcp-servers.md` (the new placement and the argument
  rule), `doc/concepts/containers.md` ("What outrig sets in the run" is currently authoritative
  and says `--userns=keep-id` unconditionally), `doc/reference/config.md`, and `SECURITY.md`.

## Config surface

```toml
[images.coding.sidecars.tools]
image = "docker.io/mcp/filesystem:latest"
view  = "primary"          # default "none"

[images.coding.mcp]
fs = { sidecar = "tools", args = ["/workspace"] }
```

and the one-liner, which is the form worth putting in the quickstart:

```toml
[images.coding.mcp]
fs = { image = "docker.io/mcp/filesystem:latest", view = "primary", args = ["/workspace"] }
```

`view` and `workspace` are mutually exclusive. The primary's view already contains the workspace
at its real path, so `workspace = "rw"` alongside it would re-bind the host source over the
thing it is meant to replace.

## Runtime behavior

The emitted `podman create` for a `view = "primary"` sidecar, following the prototype's
`22-sidecar-mcp-demo.sh`:

```
podman create --name outrig-<sid>-tools
  --userns=container:outrig-<sid>            # replaces --userns=keep-id
  --cap-add=SYS_ADMIN --cap-add=SYS_PTRACE
  -v /proc/<primary-pid>/ns:/target-ns:ro
  -v <session-dir>/outrig-enter:/outrig-enter:ro
  --entrypoint /outrig-enter
  --interactive --rm --pull=never
  docker.io/mcp/filesystem:latest
  --ns-file /target-ns/mnt --graft /mnt --cwd /workspace --
  /mnt/usr/local/bin/node /mnt/app/dist/index.js /workspace
```

Four details are load-bearing and each cost the prototype time:

- **Bind the nsfs *directory*, not the file.** Podman always adds `MS_REC` to a `-v`, and nsfs
  rejects it. `-v /proc/<pid>/ns/mnt:...` fails with `invalid argument`; the directory works.
- **`--userns=container:<primary>` replaces `--userns=keep-id`**, which is otherwise hard-coded
  at `container/mod.rs:706`. 0087 deliberately left userns unparameterized -- "a caller wanting
  `--userns=host` is a separate ask" -- and this is not that ask. It is a single derived value
  for one placement mode, not a user-facing knob, and it is required: the sidecar must be in the
  user namespace that *owns* the target mount namespace.
- **SELinux.** `append_bind_mount` appends `,Z` to every mount when `getenforce` reports
  enforcing (`container/mod.rs:748-764`). Relabeling `/proc/<pid>/ns` is wrong and will fail;
  the nsfs and helper mounts need to bypass that path.
- **The primary must be running.** A `Created` container reports `State.Pid = 0` and has no
  mount namespace. 0086 fanned sidecar bring-up out into ensure (Phase A), label merge (Phase
  B), and starts (Phase C); Phase C already runs after the primary is up, so the ordering holds
  -- but the PID read belongs in Phase C, not A, and that should be explicit rather than
  incidental.

### The argument asymmetry

This is the one thing a user has to understand, so the docs should lead with it. In that command
line, two paths mean different things:

```
/mnt/app/dist/index.js    <- a SIDECAR path: needs the graft prefix
/workspace                <- a TARGET path: used bare
```

The rule follows the declaration, not a guess about the string:

- Elements that came from **the sidecar image** -- its ENTRYPOINT and CMD -- name files in the
  sidecar's own rootfs. Prefix absolute ones with `--graft`.
- Elements the user wrote in config -- 0088's `args` -- name files in the primary. Pass bare.

That split is why 0088 is a prerequisite rather than a nicety: without a config-supplied `args`,
there is no bare side of the asymmetry and no way to tell a server which directory to serve.
`outrig-enter` itself rewrites only the program path, exactly as the prototype does; the rest of
the prefixing is OutRig's, because OutRig is the one that knows which list an element came from.

## Validation

| Case                                                   | Result                                 |
|--------------------------------------------------------|----------------------------------------|
| `view = "primary"`, entrypoint-stdio                   | accepted                               |
| `view = "primary"` + `workspace`                       | rejected -- the view already has it    |
| `view = "primary"` hosting exec-stdio                  | rejected -- out of scope for this task |
| `view = "primary"` + `capability-profile = "drop-all"` | rejected -- drops the required caps    |
| `view = "primary"`, helper unavailable                 | startup error naming the artifact      |

The `drop-all` conflict is a hard error rather than a silent re-add: a config asking for both is
asking for contradictory things, and quietly granting `SYS_ADMIN` to something that requested
`drop-all` is the worst available outcome.

## Security

This is a real posture change and the docs must say so in those terms.

A `view = "primary"` sidecar runs with `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE` and shares the
primary's user namespace. Within that namespace it can mount, and it can read the primary's
entire filesystem -- which is the point, but it means the sidecar image is now as trusted as the
primary image. `doc/concepts/mcp-trust-model.md` should say that plainly.

What bounds it:

- The capabilities are scoped to the **rootless** user namespace, not host root. They are far
  weaker than they read, and they are the same namespace the primary already runs in.
- It is opt-in per sidecar and defaults to `"none"`, so every existing config is unaffected.
- The sidecar is still a container: cgroups, network policy, and seccomp still apply, and
  `no-new-privileges` stays on by default. This is the property the host-process form of the
  technique gives up, and the reason this task takes the sidecar form instead.
- Only the mount namespace is joined. PID, network, and cgroup stay the sidecar's own.

`SECURITY.md`'s "Known boundaries" needs a line: a sidecar in this mode is inside the primary's
trust boundary, not beside it.

## Acceptance

- An e2e test drives unmodified `docker.io/mcp/filesystem` against an OutRig-launched primary
  from config alone -- no Dockerfile change, no `podman exec`, no bind mount of the workspace
  into the sidecar -- and completes an MCP handshake plus a directory listing.
- The same test asserts the sidecar sees a path that only the primary image provides and no bind
  mount could (the prototype uses `/usr/local/cargo/bin/cargo`), which is what distinguishes
  this from `workspace = "rw"`.
- Argv unit tests alongside the twelve at `container/mod.rs:879-1354`: flag order, the nsfs
  directory bind, `--userns=container:` replacing `--userns=keep-id`, and the graft prefix
  applied to image-supplied ENTRYPOINT elements but not to `args`.
- A sidecar without `view` produces a byte-identical argument vector to the one produced before
  this change.
- The graft stays invisible to the primary: `podman exec <primary> ls -A /mnt` is empty.
- Killing the primary tears down its `view = "primary"` sidecars rather than leaving them
  serving a filesystem whose container is gone.
- Each row of the validation table has a test.

## Open questions

- **Exec-stdio in a grafted sidecar.** `podman exec` joins the container init's namespaces as
  they currently are, and the init has already unshared and grafted -- so it may simply work,
  which would let a `view = "primary"` sidecar host several servers instead of one. It is a
  measurement, not a design question. Measure it, and if it holds, drop the validation error in
  a follow-up rather than guessing here.
- **`--pid=container:<primary>`.** The prototype supports it and it is simpler (the target
  becomes PID 1, no nsfs bind). Binding `/proc/<pid>/ns` instead keeps the sidecar in its own
  PID namespace, which is the better isolation default. Revisit only if the nsfs bind causes
  trouble.
- **Whether `view` belongs on the primary too** -- a second primary-like container sharing the
  first's view. No consumer; do not generalize speculatively.
- **Stale views.** Teardown handles the primary dying. A primary that is *replaced* mid-session
  has no story, but nothing in OutRig replaces one today.

## Dependencies

- ~~0088~~ -- landed. It supplies `args`, the bare side of the argument asymmetry, and it also
  made a named sidecar block able to be an entrypoint host, which is what gives `view` a place
  to live. See `plan/next/0090-config-surface-recheck.md`: the Config surface below is now
  legal as written, and the scope note wants re-deriving.
- 0089 -- `outrig-enter`, the launcher this mounts and sets as the entrypoint.

## See also

- <https://github.com/tgockel/prototype-podman-shared-fs> -- `21-sidecar-setns.sh` pins each
  requirement with a negative test; `22-sidecar-mcp-demo.sh` is the command this task emits.
- `plan/done/0080-sidecar-entrypoint-stdio.md` -- the create/start path this extends.
- `plan/done/0086-sidecar-startup-performance.md` -- the Phase A/B/C bring-up structure.
- `plan/done/0087-nested-container-runtime.md` -- the precedent for adding launch flags through
  `append_launch_flags`, and for treating a security-relevant key as opt-in with docs.
