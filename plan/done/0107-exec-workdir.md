# 0107 -- A working directory for `exec_stdio` and `exec_capture`

## Context

`Outrig::exec_stdio` and `Outrig::exec_capture` (`crates/outrig/src/outrig_.rs:1069-1086`) take
argv and env and nothing else:

```rust
pub async fn exec_stdio(&self, argv: &[String], env: &BTreeMap<String, String>) -> Result<Child>;
pub async fn exec_capture(&self, argv: &[String], env: &BTreeMap<String, String>) -> Result<Output>;
```

They delegate to `Container::exec_stdio` / `exec_capture` (`container/mod.rs:990-1008`), which
assemble the invocation in `build_exec_argv` (`:960-979`):

```rust
let mut c = Cmd::new("podman")
    .args(["exec", "-i"])
    .arg(format!("--user={}:{}", self.uid, self.gid))
    .arg("--env").arg(format!("HOME={}", userdb::home_dir(user_name)));
for (k, v) in env { c = c.arg("--env").arg(format!("{k}={v}")); }
c = c.arg(&self.name);
```

`--workdir` is never passed. A grep for `workdir` across `crates/outrig/src/` finds two hits,
neither on the exec path. So every exec lands in the image's `WORKDIR`.

## Why it matters

An embedding program that runs a build wants to run it in the checkout, and has three bad
options:

- **Wrap in a shell** -- `sh -c 'cd /workspace && ...'`. Defeats the point of the argv form,
  which exists so a shell-less image stays usable, and forces the caller to quote correctly.
- **Set `PWD`** -- changes the variable without changing the process's directory, so anything
  calling `getcwd` is unaffected. Worse than nothing, because it looks like it worked.
- **Require every command to be absolute** -- pushes the problem onto every caller and does not
  help a tool that resolves relative paths itself.

podman already supports the flag; only the surface is missing.

## Goal

Let a caller set the working directory for an exec without wrapping the command in a shell.

## Deliverables

- **The flag.** `build_exec_argv` emits `--workdir <path>` when a directory is supplied.
- **The surface.** Both `Outrig` methods and both `Container` methods take the directory.
  0095 established `ContainerCreateOptions` for exactly this kind of growth
  (`plan/done/0095-options-structs-and-sealing.md`); prefer an options struct over a fifth
  positional parameter, and prefer it over two more method variants.
- **Absent means unchanged.** Omitting the directory keeps today's behavior -- the image's
  `WORKDIR` -- so every existing caller is unaffected.
- **A missing directory is podman's error to report**, surfaced with the path in it rather than
  swallowed. Validating existence in advance would need an extra exec per call for a case
  podman already handles.
- **Docs.** `doc/concepts/containers.md`'s "What outrig sets in the run" lists the flags on the
  run path; the exec path deserves the same treatment.

## Acceptance

- An exec with a working directory set runs there: `pwd` returns it, and a relative path in
  argv resolves against it.
- The argv form works with no shell in the image -- assert against an image without `sh`, which
  is the case the whole feature exists to serve.
- Omitting it is byte-identical to today's invocation.
- A nonexistent directory produces an error naming the path.
- `library_surface` gains a case; the existing `exec_capture_runs_a_command_in_the_primary`
  (`crates/outrig/tests/library_surface.rs:534`) still passes unchanged.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean a
prototype should confirm), or **Open** (deferred).

1. **Options struct versus a parameter -- Recommended: an options struct.** `exec_*` has two
   parameters today and this is the second thing wanting to join them; a third and fourth
   (a timeout, a tty flag) are foreseeable. 0095 already set the precedent and the
   `#[non_exhaustive]` sweep makes a struct additive. Confirm the ergonomics do not become
   worse than the two-parameter call for the common case of neither.

2. **Whether the sidecar exec path needs it too -- Open.** Sidecar servers are spawned by
   `McpClient::connect_via_podman_exec_with_source`, which has its own argv assembly. If a
   sidecar server ever needs a working directory, it should reuse whatever shape lands here.

## Decisions

1. **The options struct absorbs `env`** -- fork 1 resolved, and further than "an options struct":
   `ExecOptions` replaces the `&BTreeMap` parameter rather than joining it. `ContainerCreateOptions`
   already holds its `env` that way, so keeping it out here would have meant env is inside the bag
   on create and beside it on exec, with the foreseeable `timeout` / `tty` knobs landing inside and
   `env` the lone exception. The ergonomics check the fork asked for passes: the common case of
   neither goes from `exec_capture(&argv, &BTreeMap::new())` to
   `exec_capture(&argv, &ExecOptions::new())`, the same call shape and one character longer.

   What settled it is that Rust has no default arguments, so the "third parameter" shape breaks
   every call site anyway without buying source compatibility. The only genuinely additive shape is
   a second pair of methods (`exec_capture_with`), which the Deliverables rule out. Blast radius
   was 9 call sites, 8 of them tests.

2. **Acceptance's "still passes unchanged" is about behavior, not source.** Every shape the
   Deliverables permit edits `exec_capture_runs_a_command_in_the_primary`'s argument lists. It was
   updated and still passes.

3. **A missing directory is not an `Err`.** The Deliverables call it podman's error to report,
   and the mechanism turns out to matter: `process::try_capture` returns `Err` only when the
   *spawn* fails, so podman's own non-zero exit arrives as `Ok(Output)` with the message on
   `stderr`. Verified against podman -- it names the path. So the criterion "produces an error
   naming the path" is met by a non-zero `Output::status` plus stderr, which is also what the
   existing contract ("a non-zero exit is data, not an error") requires. No outrig-side wrapping,
   and the e2e assertion is on `Output.stderr`, not `expect_err`.

4. **`--workdir` sits after the `--env` block, before the container name.** That makes the
   no-workdir argv a strict prefix of the with-workdir one, which is what
   `podman_exec_args_without_workdir_are_byte_identical` pins.

5. **`ExecOptions` is re-exported at the crate root**, unlike `ContainerCreateOptions`, because it
   is the one `container` type a root-level *signature* names. The doubling follows `config`'s
   types, which are already reachable both ways. This is a convenience, not a rule: the first
   draft justified it as "a root method's argument type should be reachable from the root", which
   is wrong twice over -- `ContainerCreateOptions` *is* named outside `container` (from `outrig_`),
   and the rule applied consistently would demand re-exporting `Container` itself, which
   `McpClient::connect_via_podman_exec` takes. The comment in `lib.rs` states the narrow version.

   The cost is real: `public-api.txt` carries the type twice, so a future field edits two blocks.

6. **`with_workdir` takes `impl Into<PathBuf>`, not `Option<PathBuf>`.** 0095 decision 9 chose
   `Option` for `with_transcript` because every producer already held one; that is not true here,
   where callers hold a literal path.

7. **Fork 2 (the sidecar exec path) stays Open.** `McpClient::connect_via_podman_exec_inner` now
   passes `ExecOptions::new().with_env(env)`, which is the shape a sidecar working directory would
   reuse if one is ever wanted. No behavior change.

8. **A shell-less test image had to be built from scratch** -- nothing in the tree had one, and
   `FROM scratch` fixtures elsewhere are config-parsing props that are never built. The new
   `build_shell_less_image` helper deletes the shell applet names from `alpine`, which is busybox
   underneath: `sh`, `cat`, and `pwd` are all symlinks to one static binary, so removing the
   shell's names leaves the rest working. alpine rather than `busybox` as the base because every
   other e2e fixture already uses it, so no second base image gets pulled. No `CMD` line: the run
   path appends `sleep infinity` after the image ref, which would override one anyway. The e2e
   test asserts `sh -c pwd` fails first, so the shell-less claim is load-bearing rather than
   assumed.

9. **The e2e test uses `/workspace/sub`, not `/workspace`.** The first draft asked for
   `/workspace` and was worthless: the run path already emits `-w /workspace`, so all three
   positive assertions passed with `--workdir` deleted from `build_exec_argv` entirely. Naming a
   subdirectory makes the no-workdir `pwd` a real contrast, and `MARKER.txt` exists only there, so
   the relative-path case fails outright if the flag does not apply. Verified by temporarily
   removing the flag and confirming the test goes red.

10. **`ExecOptions` derives only `Debug, Clone, Default`**, matching `ContainerCreateOptions`.
    `PartialEq`/`Eq` were dropped: nothing compares two of them (the tests compare argv), and on a
    public type they are SemVer commitments that would constrain the type of every future field.
    `Default` stays because `new()` needs it and clippy wants the pair.

11. **"Unset means the image's `WORKDIR`" was wrong, and the first round of docs said it four
    times.** A workspace-backed launch sets `-w` to the workspace's container path on the *run*,
    so an unset exec runs in the workspace -- on the host-mounted checkout -- not in the image's
    directory. The e2e test asserted exactly this (`pwd` with no workdir is `/workspace`) while
    the doc comments beside it claimed otherwise. `ExecOptions::workdir`, `::new`,
    `::with_workdir`, and `build_exec_argv` now say "the container's configured working
    directory", name the workspace case explicitly, and warn that a relative or destructive
    command lands on the checkout unless the directory is set. The Deliverables' "absent means
    unchanged" still holds -- unchanged is what it always was, which is not the image's `WORKDIR`.

12. **The exec doc section had to be scoped to exec-hosted processes.** Its first draft opened
    with "every real process reaches the container through `podman exec`", which is false for an
    entrypoint-stdio server: that is the image's own `ENTRYPOINT`, started by `podman create` +
    `podman start --attach`, so it gets no `--user` and no `HOME` from the exec path and runs as
    whatever user its image expects. `sidecar::bootstrap_needed` carries an explicit entrypoint-host
    exemption, so such a container may skip the runtime user bootstrap entirely. Stating the
    guarantee unconditionally would have had an operator assume mapped ids that server never
    receives. The section now names the exception, and the exception to it -- a `view = "primary"`
    sidecar, whose launcher takes the session's ids explicitly and drops to them (0102).

13. **`public-api.txt` keeps its `std::io` renderings.** Regenerating against cargo-public-api
   0.52.0 also produced `std::io` -> `core::io` churn on eight unrelated lines, the drift
   `plan/todo/README.md` already recorded as non-semantic. Those were reverted so the committed
   diff is only this task's surface change; the nightly that produces them is not pinned anywhere.

14. **Stale references in this file's Context** pointed at `outrig_.rs:1069-1086`,
    `container/mod.rs:990-1008`, `:960-979`, and `library_surface.rs:534`. The real locations at
    execution time were `outrig_.rs:1080`, `container/mod.rs:738-756`, `:708`, and
    `library_surface.rs:643`.

## Dependencies

- None.

## Consumers

- CocoClaw's phase 0006 `0006-07-exec-provider` ships a `cwd` argument that works on its shell
  form and is a pointed error on its argv form, naming this entry as the reason. When this
  lands, that asymmetry is deleted.
