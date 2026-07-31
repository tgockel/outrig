# Refuse `view = "primary"` when the podman service is remote

## Context

`view = "primary"` assumes the CLI process and the container share a kernel. Two of the flags
`append_launch_flags` emits for it say so directly (`src/container/mod.rs:1209-1222`):

- `-v /proc/<pid>/ns:/target-ns:ro` -- fine on its own. `primary_pid` comes from
  `podman inspect --format {{.State.Pid}}`, so it is already an engine-side PID resolving
  against an engine-side path.
- `-v <helper_host>:/outrig-enter:ro` -- not fine. `helper_host` is where
  `enter::materialize` wrote the launcher on the *client*
  (`src/container/enter/mod.rs:45-53`, called from `outrig-cli/src/cli/session_setup.rs:860`
  and `src/outrig_.rs:1037`), and podman resolves a bind source on the *engine*.

Nothing checks for this. `src/outrig_.rs:897` gates the whole feature on
`enter::is_available()` alone, and `validate_sidecar_spec` (`:1170-1175`, check at
`:1229-1232`) takes that as a single `helper_available: bool`. A build with the musl target
installed therefore reports the feature as available and proceeds to create a container whose
entrypoint bind names a path the engine cannot see.

The condition is already detected elsewhere: `container::podman_service_is_remote()`
(`src/container/mod.rs:90`) probes `{{.Host.ServiceIsRemote}}`, and
`direct_bootstrap_supported()` (`:81`) uses it to turn off the host-side user bootstrap. The
`view = "primary"` path simply never asks.

## Why it might matter

This is not only a podman-machine artifact. `podman --remote` against an engine reached over
SSH is an ordinary configuration on Linux, and it is the configuration a CI runner or a shared
build host is most likely to have. The failure today is a container that starts and then cannot
exec its own entrypoint, which reads like a broken image rather than an unsupported topology.

## Goal

A `view = "primary"` sidecar declared against a remote podman service fails at validation with
a message naming the reason, in the shape `OutrigError::FilesystemHelperUnavailable` already
uses -- before a container is created.

## Deliverables

- **A second input to `validate_sidecar_spec`.** `add_sidecar` (`src/outrig_.rs:896`) is already
  `async`, so it can `.await podman_service_is_remote()` and pass the result alongside
  `enter::is_available()`. Keeping the function sync and boolean-driven preserves its unit
  tests, which construct specs directly.
- **A distinct error variant.** "Built without the helper" and "the engine is not on this
  machine" have different remedies and should not share a message. The new one names the
  remote service and points at running OutRig where the engine is.
- **The same check on the library facade path**, which materializes into `self.log_dir`
  (`src/outrig_.rs:1037`) rather than the session dir but has the identical problem.
- **Ordering.** Place it beside the existing helper check, after the placement rules -- the
  comment at `src/outrig_.rs:1226-1228` explains why a spec that is wrong on its own terms
  should say so first, and that reasoning covers this equally.
- **Docs.** `crates/outrig-cli/src/mcp_self/docs/concepts/mcp-servers.md:210-218` describes when
  `view = "primary"` is unavailable and currently names only the missing-helper case.

## Acceptance

- With `CONTAINER_HOST` set to a remote engine, declaring a `view = "primary"` sidecar fails
  before `podman create` runs, naming the remote service.
- Local podman is unchanged: `podman_service_is_remote()` is memoized in a `OnceCell`
  (`src/container/mod.rs:82-88`), so the probe costs one `podman info` per process.
- A probe that cannot answer counts as remote, matching the conservative default
  `direct_bootstrap_supported()`'s doc comment already sets out for the same probe.

## Design forks

1. **Refuse or route around -- Open.** Refusing is correct now and cheap. The alternative is to
   make it work: sidecars already `podman create` (`src/container/mod.rs:1151`) and only later
   `podman start --attach --interactive` (`src/mcp.rs:171`), so there is a window for
   `podman cp <helper> <container>:/outrig-enter`, which streams over the remote API, works on
   a created-but-unstarted container, and is unaffected by where the client's filesystem is.
   `--entrypoint` is resolved at start, not create, so the ordering holds. If that lands, this
   refusal narrows to whatever is left rather than disappearing -- the primary's namespaces
   still have to be reachable.

2. **Whether the workspace bind has the same defect -- Open, and probably yes.** Every `-v` from
   `append_launch_flags` (`src/container/mod.rs:1182`) names a client path, not just the helper's.
   If so, the honest check is broader than `view = "primary"` and belongs at session start.
   Worth measuring how much podman's own client translates before deciding;
   `plan/next/windows-host-support.md` carries the same open question.

## Dependencies

- None to refuse. Fork 1's `podman cp` route is shared with
  `plan/next/windows-host-support.md` and `plan/next/macos-host-support.md`, which need it for
  the same reason.
