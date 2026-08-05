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

## Dependencies

- None.

## Consumers

- CocoClaw's phase 0006 `0006-07-exec-provider` ships a `cwd` argument that works on its shell
  form and is a pointed error on its argv form, naming this entry as the reason. When this
  lands, that asymmetry is deleted.
