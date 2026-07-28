# 0096 -- Library parity for sidecar placements and primary exec

## Context

OutRig has two consumers: the CLI (`outrig run` / `outrig mcp`), which drives a `Config`
parsed from TOML, and embedding programs, which build a `LaunchSpec` by hand. Since
`plan/done/0079-sidecar-core-exec-stdio.md` and the entrypoint-stdio work that followed, the
config path has grown three placements the library path never got:

- **entrypoint-stdio** -- an MCP entry with no `command`, where the container's `ENTRYPOINT`
  *is* the server. This is what makes off-the-shelf images like
  `docker.io/mcp/filesystem:latest` usable with zero repo-side command knowledge.
- **`args`** -- the positional arguments such a server needs, since real MCP images take their
  served directories positionally.
- **`view = "primary"`** -- an entrypoint sidecar joining the primary's mount namespace
  through `outrig-enter`, so an Alpine `mcp/filesystem` can index a Debian project's tree.

The library surface is explicit that it does not reach them. `SidecarServerSpec` carries a
`command` and nothing else, with the doc comment "Exec-stdio only: ... entrypoint-stdio has no
library surface" (`crates/outrig/src/outrig_.rs:104`). `LaunchSpec::from_config` -- the one
library entry that *reads* a `Config` -- rejects entrypoint-stdio outright rather than
lowering it (`outrig_.rs:463-465`, pinned by `from_config_rejects_entrypoint_stdio` at
`:1212`), and both construction paths hardcode `primary_view: None` (`:600`, `:740`) with the
comment "the programmatic path never hosts entrypoint-stdio servers, so it never uses the
primary-view placement." Exec-stdio sidecars built by hand set `view: SidecarView::None`
unconditionally (`:920`).

There is a second, smaller gap beside it. `Outrig` owns the primary `Container` privately and
exposes no accessor, so an embedder that wants to run a command in the primary -- the whole
point of a dev-environment image -- has to rediscover `container::attach` (`container/mod.rs:381`)
or `inspect_existing` (`:431`) plus the `LABEL_SESSION` / `sidecar_container_name` naming
scheme (`:161`, `:170`) and reconstruct a handle OutRig already holds.

The motivating consumer is CocoClaw, whose phase 0006 moves MCP servers out of the agent's
primary image entirely: the primary becomes a stock upstream image (`docker.io/rust:1.97`)
and every tool arrives as a sidecar or as `exec` into the primary. It embeds the library; it
never runs the CLI. Without these two gaps closed it can reach exec-stdio sidecars only,
which cannot serve a tool over the primary's own filesystem and cannot run a build in the
primary's toolchain.

Nothing here is new mechanism. The planner, the argv builder, the launcher, and the
validation rules all exist and are exercised by the CLI; this is about making them reachable
from the facade.

## Goal

Give the library API the sidecar placements the config path already has -- entrypoint-stdio,
`args`, and `view = "primary"` -- plus a supported way to run a command in the primary
container, so an embedding program reaches the same tool topology as `outrig run`.

## Deliverables

- **Entrypoint-stdio on the library sidecar spec.** A sidecar server declared without a
  `command`, whose container's `ENTRYPOINT` is the server, with `args` supplying its
  positional arguments. `SidecarServerSpec` (`outrig_.rs:106`) and `SidecarSpec`
  (`outrig_.rs:121`) grow the shape; see fork 1 for how.
- **`view = "primary"` on the library sidecar spec.** Replace the two hardcoded
  `primary_view: None` sites (`outrig_.rs:600`, `:740`) and the unconditional
  `view: SidecarView::None` (`:920`) with the caller's choice. Reuse `SidecarView`
  (`config/mod.rs:998`) rather than introducing a parallel enum -- `SidecarWorkspaceAccess` is
  already re-exported from the crate root (`lib.rs:26-29`) and `SidecarView` should join it.
- **Lower rather than reject in `from_config`.** `LaunchSpec::from_config` currently errors on
  any entrypoint-stdio placement (`outrig_.rs:463-465`). Once the spec can express one, it
  lowers instead, and `from_config_rejects_entrypoint_stdio` (`:1212`) is replaced by a test
  asserting the lowering. `SidecarConfig` (`config/mod.rs:940`) already carries `workspace`,
  `view`, `start`, `on-failure`, `args`, `mounts`, and `security`, so the lowering is a field
  map, not a redesign.
- **`outrig-enter` availability is a clear, early failure.** A `view = "primary"` sidecar
  built from the library must check `container::enter::is_available()` (`enter/mod.rs:35`) and
  materialize through `enter::materialize()` (`:42`), failing at launch with a message naming
  the missing `<arch>-unknown-linux-musl` artifact -- the same diagnostic the CLI gives, not a
  panic or an opaque podman error.
- **A primary exec surface on `Outrig`.** So an embedder can run a command in the primary
  without reconstructing a `Container`. See fork 2 for the shape.
- **Validation parity.** The rules in `config/validate.rs:317-342` and `:807-809` -- an
  entrypoint host serves exactly one server and cannot be `start = "manual"`;
  `view = "primary"` is mutually exclusive with `workspace` and with
  `capability-profile = "drop-all"` -- must hold for a hand-built `SidecarSpec` too, and must
  live in one place so the two paths cannot drift. Today they run only over a parsed `Config`.
- **Docs.** `doc/concepts/mcp-servers.md` and `doc/concepts/containers.md` say what each
  placement is; neither says which of them an embedding program can reach. Add that, and say
  plainly that `view = "primary"` is the same posture change from the library as from the CLI
  -- `CAP_SYS_ADMIN` and `CAP_SYS_PTRACE` in the primary's user namespace. Both files are
  symlinks into `crates/outrig-cli/src/mcp_self/docs/`; edit the targets.

## Runtime behavior

Unchanged. Selection and placement planning already happen in `container/sidecar.rs` --
`Placement` (`:31`), `SidecarPlan` (`:58`), `entrypoint_args` (`:166`),
`build_primary_view_argv` (`:194`) -- and the three-phase bring-up, the network-interceptor
attach, the flat per-session server-name namespace, and teardown ordering all sit downstream
of it. A library-built spec should reach that planner as one more input shape, so a session
launched from a `LaunchSpec` and the same session launched from a `Config` produce identical
container sets, argv, and labels.

The one genuinely new path is `Outrig::add_sidecar` (`outrig_.rs:703`) with an entrypoint-stdio
or `view = "primary"` spec: the container's lifetime becomes the server's lifetime, so a
mid-session add of such a sidecar has a different failure and teardown profile from the
exec-stdio adds that method was built for. Fork 3 covers it.

## Acceptance

- A `LaunchSpec` built entirely in code, naming `docker.io/mcp/filesystem:latest` as an
  entrypoint-stdio sidecar with `args = ["/workspace"]`, starts a session whose tools include
  that server's, with no `command` written anywhere by the caller.
- The same spec with `view = "primary"` serves the *primary* container's tree: a file created
  in the primary is visible through the sidecar's tools at the primary's path.
- On a host without the musl target installed, a `view = "primary"` launch fails with a
  message naming the missing `outrig-enter` artifact.
- A hand-built spec that sets both `view = "primary"` and workspace access is rejected with
  the same error text as the equivalent config, and the rejection is produced by the same code.
- `LaunchSpec::from_config` lowers an entrypoint-stdio placement instead of erroring; a
  session launched from a config and one launched from the equivalent hand-built spec produce
  the same container names, argv, and labels.
- An embedder runs a command in the primary container through a documented `Outrig` method,
  without naming a container or a label.
- Sessions using only exec-stdio sidecars behave identically to today.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean a
prototype should confirm), or **Open** (deferred).

1. **How the spec expresses placement -- Recommended: make the server shape an enum.**
   `SidecarServerSpec` becomes a two-variant enum -- exec-stdio (`command` + `env`) and
   entrypoint-stdio (`args` + `env`) -- with `view` on `SidecarSpec` beside `workspace`, since
   it is a container property rather than a server property, exactly as `SidecarConfig` models
   it. Rejected: an `Option<Vec<String>>` command where `None` means entrypoint, which makes
   the invalid "no command and no args on a `view = "primary"` block" state representable and
   pushes the check to runtime. Also rejected: a separate `EntrypointSidecarSpec` type, which
   duplicates `mounts` / `security` / `workspace` and doubles every `with_*` builder. Confirm
   the enum reads well through the builder chain, where `with_server` is currently infallible.

2. **The exec surface's shape -- Recommended: a narrow `Outrig::exec_stdio`, not a
   `Container` accessor.** Returning `&Container` exposes `start`, `stop`, `bootstrap_user`,
   and `attach` on an object the session owns and will drop, which invites an embedder to stop
   a container OutRig is still managing. A method mirroring `Container::exec_stdio`
   (`container/mod.rs:877`) -- argv plus env, returning the `Child` -- gives the capability
   without the footgun, and keeps the primary `Container` private. Confirm that a `Child` is
   the right currency rather than a captured-output helper; callers that want output have to
   drive three pipes and a wait, which every one of them will then reimplement. A
   `exec_capture` convenience alongside it is cheap and probably right.

3. **Mid-session add of an entrypoint sidecar -- Open.** `Outrig::add_sidecar` (`:703`) was
   built for exec-stdio, where the container outlives any one server. An entrypoint host dies
   with its server, so a mid-session add of one needs a decision about what the existing
   "failed add leaves the session usable" guarantee means when the container *is* the server.
   Deferring is safe: launch-time declaration covers the motivating consumer, and
   `add_sidecar` can reject entrypoint specs with a pointed error until this is settled.

4. **Where the shared validation lives -- Open.** The rules currently sit in
   `config/validate.rs` and run over a parsed `Config`. Moving them onto the spec types
   (so both paths validate the same objects) is cleaner but touches error types the CLI's
   diagnostics depend on. Whoever executes this should pick between lifting the rules onto
   the spec and having `validate.rs` delegate, versus keeping them where they are and running
   them over a synthesized `Config`. The requirement is one implementation, not one location.

## Dependencies

- **None hard.** Every mechanism this exposes already exists and ships in the CLI path.
- **Release timing.** CocoClaw pins a crates.io release, so this surface should land in the
  published **0.2.0** rather than a follow-on -- otherwise its phase 0006 either waits for a
  0.3 or carries an uncommittable path patch.

## See also

- `crates/outrig/src/outrig_.rs` -- `SidecarServerSpec` (:106), `SidecarSpec` (:121),
  `LaunchSpec::with_sidecar` (:427), `from_config` (:316) and its entrypoint rejection
  (:463-465), the `primary_view: None` sites (:600, :740), `Outrig::add_sidecar` (:703).
- `crates/outrig/src/config/mod.rs` -- `SidecarConfig` (:940), `SidecarWorkspaceAccess` (:972),
  `SidecarView` (:998), `SidecarStart` (:1017): the shapes the library spec should mirror.
- `crates/outrig/src/config/validate.rs:317-342`, `:713`, `:807-809` -- the placement rules
  that must hold for both paths.
- `crates/outrig/src/container/sidecar.rs` -- `Placement` (:31), `entrypoint_args` (:166),
  `build_primary_view_argv` (:194), `plan_from_config` (:242): the planner both paths feed.
- `crates/outrig/src/container/mod.rs` -- the `PRIMARY_VIEW_*` constants (:176-186),
  `attach` (:381), `inspect_existing` (:431), `exec_stdio` (:877).
- `crates/outrig/src/container/enter/mod.rs` -- `is_available` (:35), `materialize` (:42).
- `crates/outrig/tests/library_surface.rs` -- `add_sidecar_extends_tools_and_serves_calls`
  (:204), `launch_with_sidecar_starts_it` (:276),
  `from_config_resolves_and_starts_a_config_sidecar` (:334): where the new cases belong.
- `plan/done/0079-sidecar-core-exec-stdio.md` and `plan/done/0088-entrypoint-stdio-args.md` --
  where the config-side placements came from.
