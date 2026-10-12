# 0003-20 -- A binding lives as long as its session

## Context

`hosted-objects.md` settles how a session gets a hosted object. The operator declares a binding,
and OutRig installs its packages, starts its process, builds the object with its factory, describes
it to the agent, and stops it at shutdown. Nothing is bound by default. `0003-18` proved the
process, its supervision and the install on their own, and `0003-19` gave the session an owner and
a builder. This task connects them to configuration, the builder, the container and the agent's
orientation. The relay that lets agent code call the object is `0003-22`'s.

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
  that exits later in the session is `0003-22`'s to handle (its fork 3).
- **Same paths.** When any binding exists, the workspace and every extra mount are mounted at their
  host paths, and a `container-path` that differs from its host path is a config error naming the
  key. A binding process starts in the workspace's host directory, which is then the interpreter's
  working directory too, so a relative path also names one file on both sides. The mounts reach
  the container per fork 5.
- **Packages in the container.** Each binding's package directory is mounted read-only at
  `/outrig/bindings/<hash>` -- under OutRig's reserved root, beside the payload, not at its host
  path (the maintainer's call; see `## Decisions`) -- and added to the interpreter's `sys.path`
  after the standard library, so importing the library works there as it does on the host.
  With several bindings the directories go on in the order above, which is then the precedence
  of their packages: where two requirement sets hold
  different versions of one package, the container imports the version of the binding that comes
  first -- a global binding's before a repo binding's, and a repo binding's before one given
  through `bind` -- while each binding process imports its own.
- **The manifest**: each binding's name, description, and the type name of the object its factory
  returned, delivered to the interpreter at start. `runtime.bindings` answers from it in every
  kernel, opened ones included, with no request to any binding.
- **Orientation**: a line per binding with its name and description, plus `Runs one call at a
  time.` when it is declared `serialize = true`; and once for all of them, that these names are
  objects in processes on the host while an `import` of the same library gives a local copy that
  knows nothing of them; that nothing happens on a hosted object until it is awaited --
  `await repo.head.commit.hexsha` reads a chain, `await repo.remotes.origin.push()` calls,
  `async for r in repo.remotes:` iterates, and `await x._set("name", value)` and
  `await x._delete("name")` assign and delete, since an assignment statement cannot be awaited;
  that `if x:`, `len(x)` and a plain `for` on a hosted object raise and say so; that `repr`, `str`
  and `dir` answer locally and `help(git.Repo)` describes the library; and that the loop keeps
  running while the host works, so `runtime.wait` still sees messages, and a hosted object is
  never put in `asyncio.to_thread`.
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
- The orientation names each binding with its description, and says that a hosted object is
  awaited.
- A factory that raises fails start, naming the binding, and leaves no binding process running.
- e2e: **nothing outlives shutdown.** No process remains in any binding's group, and the report
  says so.
- `crates/outrig/public-api.txt` regenerated, its additions the config types, `bind` and the
  approver.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **When nobody can be asked -- Settled: refuse that binding and start the session without it
   (the maintainer's call).** Stdin is not a terminal, as in a script or CI, or an embedder set no
   approver (fork 2).
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

## Decisions

The forks were taken as recommended, except fork 1, which was open and which the maintainer
settled; fork 7's split was not needed. Fork 1: a repo-declared binding that nobody can be asked
about -- no terminal, or an embedder with no approver -- does not start, the session runs without
it, and a `binding.refused` event says which binding and how to approve it; an explicit no at the
prompt does the same. Fork 2: the library checks the store and calls the builder's `Approver`;
`run-new`'s prompts on the terminal. Fork 3: the digest covers the declaration and the
repository's root. Fork 4: every relative `{ path }` resolves against the repository's root,
whichever layer declared it. Fork 5: the builder adds the mounts to the `LaunchSpec` it launches.
Fork 6: an inherited environment loses every variable the config names through `${VAR}`.

- **Package directories are mounted at `/outrig/bindings/<hash>`, not at their host paths (the
  maintainer's call, departing from the deliverable as first written).** podman creates a mount
  target's missing parents as root, and the user bootstrap chowns only the home directory itself,
  so a host path under `~/.cache` would have left `~/.cache`, `~/.cache/outrig` and
  `~/.cache/outrig/bindings` root-owned in the container user's home. No path the agent and a
  library exchange is a package path, so nothing needs them to match. They ride a crate-private
  `LaunchSpec::packages`, chained beside the payload in `primary_launch` as shared read-only
  mounts (`,z`, since sessions on one requirement set bind the same directory), where
  `reject_reserved` still refuses a caller's mount under `/outrig`.
- **What counts as repo-declared.** A binding whose `ConfigSource` is the repo config, a
  `--config` file (`RepoFile`, which takes the repo config's place in the merge and can sit in
  the workspace) or an `image.toml` project. The global config's and a `Config` built in code --
  the operator's layer, as `embedding.md` counts it -- need none.
- **The digest** is blake3 over a versioned prefix, the root's bytes and the declaration exactly
  as the approver was shown it -- each field, its paths unresolved -- so an approval covers what
  the operator read, a reformatting or a comment in the file asks nothing, and any field's edit
  asks again. A change to how a declaration is shown asks everyone again, which errs on the safe
  side. The store is a directory, one file per digest holding the declaration and the root,
  written through a temporary file and renamed: no read-modify-write of a shared file, and a
  person can read and prune it. A store that cannot be written warns and asks again next time;
  with no state directory nothing is remembered. It sits under `$XDG_STATE_HOME`, by the rule the
  cache directory follows, now one function (`payload::xdg_root`).
- **The approver is synchronous and runs on `spawn_blocking`**, as a terminal prompt blocks;
  `run-new`'s reads the same `std::io::stdin` buffer `converse` later reads, so nothing typed
  ahead is lost. The prompt shows the declaration as an operator writes one, each key on a line
  with its value inline: `toml`'s serializer would have split `args = [{ path = "." }]` into an
  `[[bindings.repo.args]]` block.
- **Factory arguments travel on stdin**, as the binding program's first line, `{"t":"factory",
  "args","kwargs"}` -- a host's command line is readable by every user on it. `ready` now names
  the qualified type of what the factory returned, which the manifest carries, and a factory that
  raises sends `{"t":"failed","error":...}` with `format_exception_only`'s line, so the start's
  error names the binding, the factory and the exception without parsing the traceback.
- **`FactoryValue`** is deserialized through `toml::Value`: a hand-written visitor is handed a
  datetime as a one-key table. Any table holding `path` is a path tag and holds nothing else; a
  datetime, NaN or infinity is refused at parse, having no JSON spelling to cross in.
- **Names.** ASCII identifiers only, not a keyword, and not `runtime`, `asyncio`, `outrig` or a
  `__dunder__` name: `__builtins__` and the module attributes are globals in every kernel too.
  The same check runs at load, in `check()`, and at start, so a `bind` name fails before anything
  is pulled.
- **`[[workspace.mounts]].container-path` is optional and defaults to the resolved host path**,
  sidecar mounts included, so a binding session need not spell a `~`-expanded path twice.
  `MountConfig::container_path` became `declared_container_path` and
  `resolved_container_path(repo_root)`. Validation without a root checks only what it can
  resolve (`known_path`); paths are compared after `payload::lexical`, since `resolve_against`
  keeps `..`. With a binding declared, the mount list's reserved path is the workspace's host
  path.
- **Same paths in the builder.** A `LaunchSpec` from the configuration holds `/workspace` when
  nothing was declared, so the builder decides from `config.workspace.declared_container_path()`:
  declared and different is an error naming the key; otherwise the workspace's container path
  becomes its host path, which a hand-built spec's is too, silently, as `bind` documents. A mount
  whose container path differs from its host path is an error. Config validation states the
  same rule for config bindings, so the error names the key before anything starts.
- **Start order: approval, install, bindings, container, interpreter.** Bindings start before the
  container, so a factory that raises costs no container launch; every later failure stops them
  with the container, warning of a group not proven empty. Requirement sets install, and
  bindings start, all at once, their results kept in the session's order; a factory that raises
  waits for the others to come up and then stops them. Overlapping the bindings with the
  container launch was weighed in review and not taken: it would spend a launch on a factory
  that fails. `Step::Packages` and `Step::Bindings` report only when a binding exists.
- **The manifest is a process-wide `bindings` line**, handled on the reader thread beside `open`,
  queued by `Interpreter::declare_bindings` behind the greeting and before any execution; a
  parameter on `connect` would have put a line ahead of the ~87 fake-transport tests' first
  request. Package directories go on `sys.path` after the workspace and before the agent's own
  pip directory, so a library the agent installs cannot replace the version its binding runs;
  `runtime.bindings` is a read-only mapping of `Binding(name, description, type)` records.
- **The orientation's examples use the first binding's name**, not GitPython's spelling, since no
  mechanism here names a library. It describes awaiting a hosted object, which `0003-21` and
  `0003-22` make work; until then the names are not bound in a kernel, and the CLI reference
  says calling one is not built yet.
- **Shutdown** stops every binding at once after the drain and the interrupt, each with
  `STOP_GRACE`, then the container. `ShutdownReport::bindings` and `session.report` carry a
  `BindingStop` per binding -- name, group proven empty, whether `SIGKILL` was needed, survivors
  as text, never a `nix::Pid` -- and a group not proven empty joins the container's failures in
  `Stopped::NotProven`'s one reason. The supervisor's module-wide dead-code allowance is gone:
  each item only the relay will read -- the `rpc`, `event` and `decision` lines, the stdin
  sender -- carries its own `expect(dead_code)`, which fires once `0003-22` uses it.
- **Simplified after review.** `/simplify`'s four reviewers found, among smaller things: the
  builder's same-path error copied the validation error's text, and now is that error; the
  binding's stops on a failed start warned on some paths and not others, and now share one
  helper; test-only places rode production fields, and are one `Places` behind `#[cfg(test)]`;
  `lexical` moved from `python::payload` beside `resolve_against`, since config compares paths
  with it; `group_gone` and `wait_group_gone` moved to `testing.rs`. Not taken: deriving the
  workspace's same path inside `LaunchSpec::from_config`, since whether a session has a binding
  is known only after approval, at start; and delivering the manifest on the interpreter's
  command line rather than as a line, which `0003-22` can fold in when it starts passing the
  RPyC directory there.
- **Tests.** Host sessions run real bindings from local wheels: `WHEELS` gained a version and a
  body per wheel, `verfix` 1.0-4.0 and a `recorder` that writes what its factory was given.
  Repo-declared bindings are loaded through `Config::load` from files, since only loading stamps
  a source. Builder hooks (`binding_places`, test-only) point installs, pip and the approval store
  at temporary directories. The test wheels are built off the test's runtime, since building
  them unpacks the payload on a runtime of its own. The secrets test sets its variable with
  `set_var` under the agent tests' rule of a name per test.
- **Filed in `plan/next/`:** `binding-start-is-not-evented.md` -- no event says a binding
  started, with what; `listing-and-revoking-binding-approvals.md` -- the approval store is read
  and pruned by hand; `same-paths-under-a-symlinked-directory.md` -- a workspace reached through a
  symlinked parent has two spellings, and the container knows one.
- **Fixed after the PR review.** Four findings, each confirmed against the code:
  - A relative `SessionBuilder::repo_root` was used as given, so `repo_root("repo")` from two
    parent directories keyed one approval and resolved `{ path }` against the binding's own cwd.
    The root is now made absolute against the current directory (`std::path::absolute`, `..`
    kept) before approval, argument resolution or start; rejecting a relative root would have
    broken the natural `repo_root(".")`.
  - A tagged `{ path }` was resolved with `lexical`, which drops `..` together with the component
    before it -- wrong on the host when that component is a symlink. Only `.` is dropped now, and
    `..` is left for the host. `lexical` stays for comparing container paths, where podman
    resolves a mount's destination the same textual way.
  - `Binding::stop` and a failed start's cleanup awaited the leader's reap without a bound after
    `SIGKILL`. A thread in uninterruptible I/O (a hung FUSE daemon) outlasts `SIGKILL`, so the
    session never reached the container's stop. The reap now shares `KILL_WAIT` with the group
    check; an unreaped binding reports its group not empty -- the leader is still in it -- so
    the session is not proven stopped and goes on to the container, and `kill_on_drop` with
    tokio's orphan queue reaps it later. Not tested directly: putting a process into
    uninterruptible sleep needs a FUSE mount or root.
  - `binding.refused` was missing from the event sweep in `scripts/render-session-test.py`,
    which CI's `render-session` job checks against the catalog and `events.md`.
