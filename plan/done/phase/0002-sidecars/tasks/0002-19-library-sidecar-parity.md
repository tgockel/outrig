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

## Decisions

- **Fork 1 resolved as recommended: `SidecarServerSpec` is a two-variant enum**
  (`ExecStdio { command, env }` / `Entrypoint { args, env }`) with `view` on `SidecarSpec`,
  mirroring `SidecarConfig`. Both variants are sealed per 0094 Decision 6, so they ship
  constructors (`exec` / `entrypoint`, renaming `new` for symmetry with `McpServerSpec`),
  `with_env`, and `command()` / `args()` / `env()` / `is_entrypoint()` accessors. The builder
  chain stays infallible and the two `with_*_server` families coexist; "an entrypoint host
  serves exactly one server" remains a validation rule rather than a type invariant. Rejected:
  a hosting enum on `SidecarSpec` itself, which would make that rule structural but breaks the
  public `servers` map and makes `with_server` / `with_entrypoint_server` silently conflict.
- **`args` lives only on the entrypoint variant, not on `SidecarSpec`.** The config carries it
  in two places because a block and an entry can each declare it; `sidecar::entrypoint_args` is
  the one place that picks between them, and the lowering collapses the choice there. Mirroring
  both slots into the library would reintroduce an ambiguity the library does not have.
- **`start` and `on-failure` are deliberately absent from `SidecarSpec`.** A library sidecar
  starts when `add_sidecar` is called, and `with_sidecar` was already documented as abort-only.
  This makes `SidecarEntrypointNotAuto` structurally impossible on the library path rather than
  a fourth rule to keep in sync.
- **Fork 3 resolved as "allow", not deferred.** `Outrig::launch` starts launch-time sidecars by
  calling `add_sidecar`, so supporting entrypoint specs there is strictly less code than
  rejecting them (which would need a second launch path). The failure contract is unchanged:
  create -> attach interceptor -> connect, and any failure detaches, stops the container, and
  leaves the session as it was, which `failed_add_sidecar_leaves_session_usable` still pins.
- **Fork 4 resolved by lifting, not by synthesizing a `Config`.** `ConfigValidationError` turned
  out to be span-free, path-free and provenance-free, and the CLI only ever prints `Display`, so
  the rules move into `pub(crate)` functions taking the minimal borrowed pieces --
  `check_view_exclusions`, `check_entrypoint_hosting`, `check_sidecar_name`,
  `check_sidecar_image` -- which both `validate_sidecar` and the facade's
  `validate_sidecar_spec` call. Two unit tests assert the library's message is byte-identical to
  the equivalent config's by generating the latter, not by copying a literal.
  - Where `SidecarEntrypointNotAlone` / `SidecarViewRequiresEntrypoint` name an *image-config*,
    the library passes its podman image ref into that slot -- a hand-built spec has no
    image-config to name. Rejected: reshaping those variants to a neutral `scope` field, which
    churns config-path error text for no gain.
  - `validate_sidecar_spec` stopped hand-copying `SidecarNameInvalid`'s message, and
    `is_valid_sidecar_name` went away as its only caller.
- **`helper_available` is a parameter, not an `enter::is_available()` call inside the check.**
  Threading it in makes "a build without the musl target rejects a `view = "primary"` spec,
  naming the artifact" a CI-runnable unit test on a host that *does* have the helper. The check
  runs after the placement rules, so a spec that is wrong on its own terms says so rather than
  blaming the toolchain.
- **`outrig-enter` materializes into `log_dir`** (fork 4 of the plan's questions). It is the one
  writable directory a `LaunchSpec` names and `Outrig` keeps, so mid-session adds need no new
  field. `add_sidecar` creates it first: the log dir is otherwise created lazily by the first
  MCP connection, which for an entrypoint server happens *after* the helper is needed -- caught
  by the e2e, not by review.
- **`entrypoint_create_args` in `container::sidecar` is called by both crates.** It folds in the
  `PRIMARY_VIEW_GRAFT` / `_NS_MOUNT` / `_NS_FILE` plumbing that `create_one_entrypoint_sidecar`
  spelled inline, so "a config-launched and a spec-launched session produce the same argv" is a
  shared code path rather than a coincidence, and is unit-testable without podman.
- **`exec_capture` returns `std::process::Output`** rather than a new type: it already carries
  exactly `status` / `stdout` / `stderr`, needs no `#[non_exhaustive]` deliberation, and adds
  nothing to the surface being frozen. It is built on `process::try_capture`, not `run_capture`,
  so a non-zero exit is data rather than an error.
- **Fixed a pre-existing `view = "primary"` bug that blocked the acceptance criterion.**
  `build_primary_view_argv` graft-prefixed the payload's *program*, but `outrig-enter` opens it
  pre-setns (while the sidecar rootfs is still at `/`) and re-applies the graft itself when
  invoking the loader -- so an absolute `ENTRYPOINT` was looked for under the graft twice. The
  program is now passed bare. This was verified failing on an unmodified `43dea081` worktree
  before the change, so it is not a regression from this task; it is fixed here because the
  library's flagship placement could not otherwise be demonstrated at all.
  - The *other* half of that bug is left alone and filed as
    `plan/next/primary-view-relative-entrypoint.md`: the launcher does no `PATH` search, so an
    image whose `ENTRYPOINT` is a bare `node` -- which is `docker.io/mcp/filesystem:latest`,
    the docs' quickstart -- still cannot start. `crates/outrig-cli/tests/primary_view_e2e.rs`
    consequently remains red, exactly as it is on trunk. Fixing it means changing the
    standalone musl launcher, which is 0089's territory and well outside this task.
  - The library's `view = "primary"` e2e therefore runs a `docker.io/mcp/filesystem:latest`
    derivative that restates the same program absolutely, which exercises the placement end to
    end without depending on the unfixed half. The plain entrypoint-stdio e2e uses the upstream
    image unmodified -- with no view there is no graft, so podman resolves the relative
    `ENTRYPOINT` normally, and the acceptance criterion's exact case is covered as written.
- **The entrypoint e2e uses the real image, not the `mcp-entrypoint` fixture.** "An unmodified
  off-the-shelf MCP image needs no repo-side command knowledge" is the claim, and a fixture with
  a hand-written ENTRYPOINT would not test it. The louder "the args did not arrive" signal that
  fixture provides (0088 made its `entry.sh` exit 64 on empty argv) is already covered on the
  config path by `named_sidecar_entrypoint_host_serves_workspace_from_args`.

## Decisions from the `/simplify` pass

- **`build_primary_view_argv` builds the payload in one pass** rather than prefixing everything
  and stripping the graft back off `argv[0]`. The strip formulation was wrong, not just
  roundabout: when the image declares no `ENTRYPOINT` and `args` is non-empty, `argv[0]` is a
  *config* arg -- bare by design, naming a path in the primary's view -- and a legitimate
  `args = ["/mnt/data"]` would have been silently rewritten to `/data`. Pinned by
  `primary_view_argv_does_not_rewrite_a_config_arg_under_the_graft_point`.
- **The argv contract is now stated on the launcher too** (`enter/launcher.rs` module docs):
  `PROGRAM` is in the sidecar's coordinates and ungrafted, `ARGS...` are in the target's. It was
  written down only on the producer side, and `plan/next/primary-view-relative-entrypoint.md`
  sends the next reader straight into the consumer.
- **The empty-workspace `--cwd` fallback moved into `entrypoint_create_args`**, which now takes
  `&Path`. `--cwd ""` makes the launcher `chdir("")` and die; the guard belongs to the function
  that owns the flag, not to the one caller that can currently produce it (the CLI always has a
  container path, the library need not).
- **`bootstrap_needed` absorbed the entrypoint-host exemption** as a leading parameter, so
  "an entrypoint host never bootstraps" is stated once instead of in `sidecar_needs_bootstrap`
  and `add_sidecar` separately.
- **`with_entrypoint_server_env` was replaced by a general `with_server_spec(name, server)`**
  before it shipped. Four combinatorial builders for two transports x with/without env is
  surface that a release freezing the API should not take on; the general form composes with
  `SidecarServerSpec::{exec,entrypoint}(..).with_env(..)` and absorbs any future variant.
- **`config::validate` stayed a private module.** The four shared checks are re-exported by name
  through the existing `pub(crate) use` list rather than opening the whole module, which would
  have exposed ~20 unrelated helpers with no per-item decision.
- **`entrypoint_create_args` matches `SidecarView::None` explicitly** rather than with a
  catch-all: `#[non_exhaustive]` does not suppress exhaustiveness inside the defining crate, and
  `SidecarView::as_str` already establishes that convention -- a third view mode should be a
  compile error here, not a silent "no view".
- **The reserved server name is now rejected on the library path too.** `outrig` is refused by
  config validation and by both label readers, but a `SidecarSpec` could host a server by that
  name and collide with the built-in `outrig__*` tools. Found while making the rest of
  `validate_sidecar_spec` delegate; small enough to fix in place.
- **Not done, deliberately:** hoisting the remaining parallel structure between
  `Outrig::add_sidecar` and the CLI's `create_one_entrypoint_sidecar` into a shared
  container-creation helper. What is left parallel is mechanical field mapping from two
  genuinely different source types, and sharing it would mean threading transcripts, progress
  spans, `on-failure` routing and the `--env` overlay through as `Option`-shaped parameters --
  and would give up the CLI's amortization of `enter::materialize` + `primary.pid()` across
  concurrently-started sidecars. The parts where drift would be *silent* (argv text, the
  placement rules, the bootstrap exemption) are shared; the parts where it would be a compile
  error are not.
