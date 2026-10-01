# 0003-20 -- A binding lives as long as its session

## Context

`hosted-objects.md` settles how a session gets a hosted object. The operator declares a binding,
and OutRig installs its packages, starts its process, builds the object with its factory, describes
it to the agent, and stops it at shutdown. Nothing is bound by default. `0003-18` proved the
process, its supervision and the install on their own, and `0003-19` gave the session an owner and
a builder. This task connects them to configuration, the builder, the container and the agent's
orientation. The relay that lets agent code call the object is `0003-21`'s.

A declaration, with GitPython as the example:

```toml
[bindings.repo]
description = "The project's Git repository, on the host, with your remotes and credentials."
requires    = ["GitPython==3.2.0"]
factory     = "git:Repo"
args        = [{ path = "." }]
serialize   = true
```

`factory` names a callable as `module:callable`, and `args` and `kwargs` are literal values. A path
is written in the tagged form `{ path = "..." }` and reaches the factory as an absolute host path,
because OutRig cannot tell which strings are paths and the operator can. `serialize = true` says
the library is not thread-safe, and the binding then runs one call at a time across all its
connections (`0003-17`); it is `false` when absent. GitPython needs it.

Three settled decisions determine the rest:

- **Approval.** A factory is code the host runs as the user at session start, and a repository's
  config is repository content: a cloned project contains one, and the agent can write it through
  the workspace mount. So a repo-declared binding starts only after the operator approves that
  exact declaration -- prompted once, remembered by a digest of the declaration, and prompted again
  after any edit. A binding from the global config or from the builder needs no approval, since
  the operator or the embedder wrote it. `SECURITY.md` holds a repository to the same kind of rule
  for `[network]`.
- **Same paths.** When a session has a binding, the host directories it mounts -- the workspace and
  every `[[workspace.mounts]]` entry -- appear in the container at their host paths. A path the
  agent writes and a path the library reads then name the same file, with no translation. A
  configured `container-path` that differs from its host path cannot satisfy that, and is a config
  error.
- **Presentation.** The agent learns of a binding from its orientation -- name and description --
  and from `runtime.bindings`, which answers from a manifest the interpreter holds, so asking never
  reaches the binding. A name that is not an identifier, or that collides with `runtime`,
  `asyncio`, `outrig` or another binding, fails start rather than shadowing anything.

What exists: `config/merge.rs` merges each global and repo map with the repo's entry replacing the
global one of the same name. The maps are `BTreeMap`s, so the merged config keeps no declaration
order, and the order of entries is their names'. `stamp_source` in `config/mod.rs` records the
declaring file's `ConfigSource` on images, the workspace and mounts, and a binding needs the same,
both to resolve its paths and to tell a repository's binding from the global config's. `Workspace`
and `MountSpec` each carry a host path and a container path. `_open_imports` in `interpreter.py`
appends to `sys.path` after the standard library. `agent/orientation.rs` writes the preamble.

## Goal

A binding declared in config or through the builder is approved when it must be, installed,
started before the agent, described to it, and stopped with the session; and every directory the
session mounts has one path on both sides.

## Deliverables

- **`[bindings.<name>]`** -- `description`, `requires`, `factory`, `args`, `kwargs`, `serialize`,
  and the tagged `{ path = "..." }` -- parsed, validated, stamped with its `ConfigSource`, and
  merged by the existing rule. Validation checks the `module:callable` form, that the name is a
  Python identifier and not a keyword, that `serialize` is a boolean when present, and the form of
  every `{ path }`; relative paths resolve per fork 4. A repo entry that replaces a global one of
  the same name is repo-declared, and needs approval like any other.
- **`serialize` reaches the binding process.** Each binding process is started with its
  declaration's `serialize` value, `false` when absent, and `0003-17`'s server loop reads it:
  `true` runs one call at a time across the binding's connections, `false` a call per connection.
- **An order for bindings**: the global config's, then the repo config's, then the builder's, and
  by name within each. A repo entry that replaces a global one takes its place among the repo's.
  The collision check runs on the merged set, after the global, repo and builder bindings are
  combined: a name that is `runtime`, `asyncio` or `outrig`, or a builder `bind` that repeats a
  config binding's name, fails start.
- **Approval of repo-declared bindings.** The digest is taken over the declaration as written
  (fork 3) and stored under the user's state directory, `$XDG_STATE_HOME/outrig/`, or
  `~/.local/state/outrig/` without it. The builder takes an approver (fork 2), and `run-new`'s
  approver shows each unapproved declaration whole at session start, before anything is installed,
  and asks. A binding that is not approved does not start, and the session says which binding and
  why.
- **`bind(name, spec)` on `0003-19`'s builder**, taking what a `[bindings]` entry holds plus an
  optional environment. With none, the binding inherits the owner's (`0003-18`), less what fork 6
  removes.
- **Start order**: approval, install, then the binding processes started and each factory run,
  then the interpreter. A factory that raises fails start with an error naming the binding and the
  exception's type and message, and the bindings already started are stopped. A binding process
  that exits later in the session is `0003-21`'s to handle (its fork 3).
- **Same paths.** When any binding exists, the workspace and every extra mount are mounted at their
  host paths, and a `container-path` that differs from its host path is a config error naming the
  key. A binding process starts in the workspace's host directory, which is then the interpreter's
  working directory too, so a relative path also names one file on both sides. The mounts reach
  the container per fork 5.
- **Packages in the container.** Each binding's package directory is mounted read-only at its host
  path and appended to the interpreter's `sys.path` after the standard library, so importing the
  library works there as it does on the host. With several bindings the directories go on in the
  order above, which is then the precedence of their packages: where two requirement sets hold
  different versions of one package, the container imports the version of the binding that comes
  first -- a global binding's before a repo binding's, and a repo binding's before one given
  through `bind` -- while each binding process imports its own.
- **The manifest**: each binding's name, description, and the type name of the object its factory
  returned, delivered to the interpreter at start. `runtime.bindings` answers from it in every
  kernel, opened ones included, with no request to any binding.
- **Orientation**: a line per binding with its name and description, and once for all of them,
  that the name is an object on the host while an `import` of the same library is a local copy,
  and that a call blocks its kernel, with the whole expression in a worker --
  `await asyncio.to_thread(lambda: repo.remotes.origin.push())` -- as the form that does not
  (`0003-17`). A binding declared `serialize = true` is marked as running one call at a time.
- **`lifecycle.md`'s binding-process row**, true of the implementation and tested: after the drain,
  `SIGTERM`, the grace, `SIGKILL` to the group, and the result in `0003-19`'s `ShutdownReport`. A
  binding whose process already exited is stopped the same way, since what it started can still
  be running in its group.
- **`SECURITY.md`**: a "Known boundaries" entry saying that a binding acts with the host user's
  authority, including whatever its library runs on the host, and that with no policy configured
  every request runs and is published as events (`boundary-policy.md`'s default, allow); and an
  in-scope item, a repo-declared binding that starts without the operator's approval of that
  declaration.

## Acceptance

- **Collisions fail start.** A binding named `runtime`, `asyncio` or `outrig`, and a builder
  `bind` that repeats a config binding's name, each fail start with an error naming the collision.
  A repo binding that replaces a global one of the same name is not a collision.
- **An unapproved repo binding does not start**, and the session says which binding and why. After
  approval, the next session starts it without asking.
- **An edit asks again.** Changing any field of an approved repo declaration prompts again; the
  same declaration in the global config prompts not at all.
- **A repo binding that replaces a global one is asked about.** With `repo` in the global config
  and a different `repo` in the repo config, the session asks for approval of the repo's
  declaration, and until it is given no binding named `repo` starts.
- **A relative `{ path }` resolves as fork 4 settles.** A fixture factory that records its
  arguments receives, for `{ path = "." }`, the repository's root with the recommendation --
  whether the binding is declared in the repo config, in the global config, or given through
  `bind`.
- **A binding does not inherit OutRig's own secrets**, per fork 6. Under a config whose provider
  key is `${TEST_KEY}`, a fixture factory that records its environment, in a binding given no
  environment, finds no `TEST_KEY`, and finds another variable the owner's environment holds.
- e2e: **the requirement imports in the container**, from the read-only package directory.
- e2e: **the binding order decides which version the container imports.** With `z` in the global
  config, `b` and `a` in the repo config, and `c` given through `bind`, each requiring a different
  version of one fixture package, the container's `sys.path` holds their package directories in
  the order `z`, `a`, `b`, `c`, the container imports `z`'s version, and each binding process
  imports its own.
- e2e: **a path names one file.** A file the agent's code writes in the workspace appears at the
  same absolute path on the host, and `os.getcwd()` in the agent's code returns that host path.
- A config with a binding and a `container-path` that differs from its host path fails validation,
  naming the key.
- **`serialize` reaches the binding, and both values hold.** Through `0003-16`'s test relay and
  `0003-17`'s recording fixture method, two kernels' calls to a fixture binding declared
  `serialize = true` never overlap on the host; to the same binding declared `serialize = false`,
  and again with the key absent, they overlap. A declaration with `serialize = "yes"` fails
  validation, naming the key.
- **`runtime.bindings` asks nothing of the binding.** In the primary and in a kernel opened after
  start, it answers with every binding's name and description while the binding process is stopped
  with `SIGSTOP`.
- The orientation names each binding with its description.
- A factory that raises fails start, naming the binding, and leaves no binding process running.
- e2e: **nothing outlives shutdown.** No process remains in any binding's group, and the report
  says so.
- `crates/outrig/public-api.txt` regenerated, its additions the config types, `bind` and the
  approver.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **When nobody can be asked -- Open.** Stdin is not a terminal, as in a script or CI, or an
   embedder set no approver (fork 2).
   - Refuse that binding and start the session without it, with output naming the binding and how
     to approve it. A script keeps running and does less, and an agent whose instructions name the
     binding finds it missing.
   - Fail the session, naming the binding and how to approve it: run once interactively, or move
     the declaration to the global config. A script stops at once, and every non-interactive use of
     a repository fails from the day the repository adds a binding.
2. **Where approval is asked -- Recommended: the library checks the store and calls an approver the
   builder takes; the CLI's approver prompts on the terminal.** An embedder that loads a
   repository's config then gets the rule without writing a prompt, and one that sets no approver
   gets fork 1's answer. With the check in the CLI alone, an embedder could start a repo-declared
   binding that nobody approved.
3. **What the digest covers -- Recommended: the declaration and the repository's root.** Over the
   declaration alone, approving `args = [{ path = "." }]` in one repository approves the same text
   in every other repository, where `.` is another directory. Including the root asks once per
   repository.
4. **Where a relative `{ path }` resolves -- Recommended: against the repository's root, whether
   the binding comes from the repo config, the global config or `bind`.** For a repo-declared
   binding the existing rule, the base directory of the declaring file's `ConfigSource`, already
   gives the root. For the global config it gives the directory holding the global config file,
   so a global `args = [{ path = "." }]` would name that directory. A binding in the global config
   is there to apply to every session's project, so `.` naming the project is the useful reading.
   A binding given through `bind` has no declaring file, and an entry without a `ConfigSource`
   already resolves against the repository's root (`source_base_dir` in `config/mod.rs`). The
   cost is that `[bindings]` becomes the one place in the global config whose relative paths do
   not resolve against that file's directory, which its documentation has to say.
5. **How the mounts reach the container -- Recommended: settled with `0003-19`'s fork 2, by the
   builder adding them to the `LaunchSpec` it launches.** A running container cannot gain a mount.
   If `0003-19` takes a launched `Outrig` instead, a binding given through `bind` arrives after the
   mounts are fixed, and the remaining options cost more: the launch is given the bindings too, so
   an embedder declares each one twice; or the whole bindings cache is mounted whenever a session
   may have bindings, so every requirement set ever installed on the machine is readable from the
   container.
6. **Whether a binding inherits the variables OutRig's config names through `${VAR}` -- Recommended:
   no; each one is removed from an inherited environment.** Under the CLI a binding inherits the
   user's environment (`0003-18`), and that environment holds OutRig's provider keys, which the
   config names as `${VAR}` and the secret resolver reads. A binding has no use for them, and
   whatever a binding's process can read, agent code can often reach through its library:
   `repo.git.execute(["printenv", "ANTHROPIC_API_KEY"])` prints one. So the variables the loaded
   config names through `${VAR}` -- in provider keys, MCP server environments and image build
   arguments -- are removed before the binding starts, and only from an inherited environment: one
   given through `bind` is used exactly. The cost is that a hosted library that needs a variable the
   config also names cannot get it under the CLI; the operator then points the config at a variable
   of another name holding the same value.
7. **If this is too large for one branch -- Recommended: keep it together unless execution shows
   otherwise.** If it does, repo-declared approval -- the digest store, the approver and the
   prompt -- becomes a task of its own. Until that task is done, a repo-declared binding is refused
   at start with a message naming its `[bindings.<name>]` entry in the repo config and saying that
   only the global config and `bind` can declare a binding yet.

## Dependencies

- **Hard: 0003-17, first half.** The thread per connection in the binding process and the
  one-call-at-a-time mode that `serialize` selects, and the recording fixture the acceptance
  reuses. Its second half, the service measurements, may be deferred without blocking this task.
- **Hard: 0003-18.** The binding process, its supervision, and the install.
- **Hard: 0003-19.** The builder `bind` extends, and the close sequence and `ShutdownReport` the
  binding's row reports into.

## See also

- `plan/phase/0003-python/hosted-objects.md` -- a binding, its packages, same paths, and
  presentation.
- `plan/phase/0003-python/security.md` and `SECURITY.md` -- what a binding grants, and the entry
  this task adds.
- `plan/phase/0003-python/lifecycle.md` -- the binding-process row.
- `plan/phase/0003-python/discovery.md` -- bindings in the preamble.
- `crates/outrig/src/config/mod.rs`, `crates/outrig/src/config/validate.rs` and
  `crates/outrig/src/config/merge.rs` -- `stamp_source`, `ConfigSource`, validation and the merge
  rule.
- `crates/outrig/src/agent/orientation.rs` -- the preamble the orientation lines join.
- `plan/next/hosted-effect-confinement.md` -- confining what a binding's library runs on the host,
  deferred.
