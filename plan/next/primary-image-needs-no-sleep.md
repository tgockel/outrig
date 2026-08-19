# Stop requiring a `sleep` (and an absent `ENTRYPOINT`) in the primary image

## Context

`build_podman_run_cmd` (`crates/outrig/src/container/mod.rs:878-890`) ends every primary and
exec-stdio sidecar launch with:

```rust
append_launch_flags(cmd, launch, selinux)
    .arg(image.as_str())
    .args(["sleep", "infinity"])
```

Trailing arguments after the image ref are the container's command, so OutRig already overrides
whatever `CMD` the image declares -- the `sleep infinity` convention in the templates is a
courtesy to a plain `podman run <image>`, not something the session depends on. The e2e suite's
shell-less fixture (`crates/outrig/tests/library_surface.rs:96-110`) states this outright and
declares no `CMD` at all.

What the appended command does do is put two requirements on the image that nothing else in the
run path imposes:

1. **The image must not set an `ENTRYPOINT`.** podman *appends* trailing arguments to an
   exec-form `ENTRYPOINT` rather than replacing it, so such an image runs
   `<entrypoint> sleep infinity`.
2. **The image must ship a `sleep` that parses `infinity`.** GNU coreutils always has; busybox
   only since 1.30 (bug 11586, fixed January 2019, first in Alpine 3.10 that June). An older
   busybox exits immediately with `sleep: invalid number 'infinity'`, and `FROM scratch` or a
   sufficiently minimal distroless base has no `sleep` at all.

Both are one flag away from gone. `append_launch_flags` already passes `--entrypoint` on the
`view = "primary"` sidecar path (`mod.rs:992-994`); the primary path simply never does.

## Why it is worth doing

The immediate payoff is that an arbitrary base image -- `ubuntu:26.04`, a vendor's SDK image, a
distroless runtime -- becomes usable as a primary without knowing anything about OutRig's
conventions. That is already the *intended* story: `concepts/containers.md` documents an
`image-name` block against a stock `ubuntu` tag, and the built-in default exists precisely to
make a config-less repo work. Today that story holds only for images that happen to satisfy the
two rules above, and `builtin_image/default.toml` had to pick its base partly on those grounds.

The secondary payoff is diagnostic. Both failures are silent at the point they occur: `podman
run -d` succeeds and prints a container id, the container exits milliseconds later, and the
user's first symptom is the *next* step failing --

    container "outrig-<sid>" has no running namespaces (not running, and not
    materialized by `podman init`)

(`mod.rs:587-592`) -- which names neither cause. Removing the causes is better than teaching
that message to guess at them.

## Sketch

Add a second statically linked musl helper beside `outrig-enter`, and set it as the primary's
`--entrypoint`:

- **`outrig-pause`**: `fn main() { loop { unsafe { libc::pause() } } }`, near enough. It needs no
  argument parsing, no namespace work, and none of `outrig-enter`'s ELF or `PATH` machinery.
  Reaping is worth a thought -- as pid 1 in the container's PID namespace it inherits orphans --
  though nothing OutRig starts is a child of it (every server arrives by `podman exec` or is the
  container process of its own sidecar), so a `waitpid` loop is defensive rather than required.
- **Build**: `crates/outrig/build.rs` already compiles one such helper with a single `rustc`
  invocation and hands it to `include_bytes!` via `OUT_DIR`; a second target is a few lines. The
  `OUTRIG_REQUIRE_ENTER` / degradation / `rerun-if-changed` scaffolding is all reusable.
- **Placement**: `container::enter::materialize` writes the launcher into the session dir and
  `append_launch_flags` binds it `:ro`. The pause helper wants the same treatment, at a distinct
  mount path, plus `--entrypoint <path>` and *no* trailing command.
- **Degradation**: unlike `outrig-enter`, the fallback here is free and total -- when the musl
  target was absent at build time, append `sleep infinity` exactly as today. So this needs no
  user-visible warning and no feature gating; it is strictly a widening of what works.

The cheap interim, if the helper is not wanted, is `--entrypoint sleep` with a trailing
`infinity`. That fixes the `ENTRYPOINT` half alone and leaves the `sleep` dependency. Prefer
that spelling to `--entrypoint=""`: podman has repeatedly mishandled the empty-string form
(containers/podman#572, #6935), whereas a one-word entrypoint plus a one-word command needs no
JSON quoting.

## What else the image still needs

Worth stating so this is not mistaken for "any image now works". After this change a primary
still needs, in rough order of how often it will bite:

- **`/etc/passwd` and `/etc/group`, and a writable `/etc`.** `bootstrap_user` opens both
  `O_RDWR|O_APPEND` inside the container's mount namespace
  (`container/namespace.rs:186-195`); every failure is fatal, since `d79f837` deleted the
  `podman exec` fallback. `/home/<user>` must be creatable too.
- **A shell, but only under `--network audit` / `--network filter`.** `install_audit_resolv_conf`
  (`network.rs:915-929`) execs `sh -c 'printf ... > /etc/resolv.conf'` as uid 0.
  Entrypoint-stdio containers already skip it (they get `--dns` at create time); the default
  network mode never runs it. Making it argv-shaped, or reusing the namespace-write path the
  user bootstrap already has, would remove the last shell dependency. Unfiled; note that
  `plan/todo/0117-attach-and-detach-are-a-true-inverse-pair.md` snapshots and restores the same
  file, so whoever touches this write path should read that task first.
- **`/mnt` and `/proc`, for `view = "primary"` sidecars only.** The launcher's own error text
  asks for them by name.

The first is the interesting one: it is what would still stop a true `FROM scratch` primary, and
it is a much larger question (a synthesized `/etc` overlay?) than this entry. Not in scope; noted
so the boundary is explicit.

## Acceptance

- A primary image with a non-`sleep` `CMD` starts and runs a session normally.
- A primary image that sets an `ENTRYPOINT` starts and runs a session normally, with its
  `ENTRYPOINT` not executed.
- A primary image with no `sleep` binary at all (e.g. busybox with the applet removed, the trick
  `build_shell_less_image` already uses for the shell) starts and runs a session normally.
- A build without the musl target still starts sessions, via the `sleep infinity` fallback.
- `podman run` argv unit tests cover both shapes, as they do today for the launcher.

## Docs

The claim that OutRig relies on the image's `CMD` was corrected ahead of this work, so the
current docs describe the appended-command behavior and name `ENTRYPOINT` (not `CMD`) as the
instruction to avoid. When this lands, the "Don't set an `ENTRYPOINT`" section of
`concepts/containers.md` loses its reason to exist and the `sleep`-availability paragraph goes
with it; `usage/run.md` step 4 and the built-in-default rationale in `reference/config.md` both
name the appended command and would need revisiting. `validate_dockerfile`'s
`entrypoint_takes_args` warning (`mcp_self/validate.rs`) should be dropped, and `cmd_ignored`
re-examined -- with the entrypoint set by OutRig, an image's `CMD` is inert for a different
reason but still inert.

## See also

- `crates/outrig/src/container/mod.rs` -- `build_podman_run_cmd` and `append_launch_flags`'s
  existing `--entrypoint` emission.
- `crates/outrig/build.rs` -- the single-`rustc` musl helper build this would extend.
- `crates/outrig/src/container/enter/mod.rs` -- `is_available` / `materialize`, the
  embed-and-place pattern to copy.
- `crates/outrig/tests/library_surface.rs` -- `build_shell_less_image`, both a precedent for the
  test fixture and an existing statement that no `CMD` is needed.
- `plan/todo/0117-attach-and-detach-are-a-true-inverse-pair.md` -- the other work on the
  `resolv.conf` write; it absorbed the buffer entry this used to point at.
