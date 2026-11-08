# 0003-43 -- Application code is provisioned before construction

## Context

Two mechanisms put Python an owner chose into the interpreter today. A binding's `requires` is
installed once into a cache keyed by the requirement set, wheels only and each wheel checked pure
(`install::install`, `crates/outrig/src/python/install.rs:98-118`; `check_pure`, `:353-395`;
`bindings_dir`, `:80-82`), mounted read-only at `/outrig/bindings/<hash>`
(`crates/outrig/src/harness/bindings.rs:200-225`, `container_path` `:452-454`), declared to the
interpreter in a `bindings` line (`declare`, `:392-404`; `bindings_message`,
`crates/outrig/src/python/host.rs:661-671`), and put on `sys.path` after the workspace and before
the agent's own installs (`_bindings`, `crates/outrig/src/python/outrig/_interpreter.py:6176-6204`).
A skill's files come from a host-side `SkillSource` (`crates/outrig/src/harness/skills.rs:64-75`),
bounded by a `DirectorySource` that follows no link (`:120-133`), served over the pipe on demand
(`serve`, `:1241-1280`) to a `meta_path` finder that serves `outrig_skills` and nothing else
(`crates/outrig/src/python/outrig/skills.py:423-431`, `:830-859`), with dependencies checked and
never installed (`_check`, `:483-528`) and import failures coded `import-failed`,
`native-extension-unavailable` and the rest (`skills.md`). OutRig's own package is written once
into a digest-named directory and mounted at `/outrig/lib`
(`crates/outrig/src/python/payload.rs:59-70`, `:162-209`). The interpreter runs `-I`, and `main`
and `_open_imports` set the `sys.path` order
(`_interpreter.py:6725-6744`, `:1350-1377`); `_compiled_cause` is `0003-10`'s import diagnostic
(`:1447`). A class's digest is a SHA-256 of its module's file (`_digest`,
`crates/outrig/src/python/outrig/_agents.py:432-453`).

An owner supplies things to a session through `SessionBuilder` -- `bind` at
`crates/outrig/src/harness/builder.rs:225`, `skill_source` at `:278` -- and a host program through
`outrig.Session(workspace, mounts, grace)` (`crates/outrig/src/python/outrig/_host.py:703-719`),
whose `session.start` request the engine reads (`crates/outrig/src/owner/mod.rs:19-24`;
`Start`, `crates/outrig/src/owner/connection.rs:130-137`; `Request` and `prepare`,
`crates/outrig/src/owner/session.rs:60-78`, `:343`). Nothing today takes a package of the owner's
and makes it importable in a kernel; `python-package-deployment-assessment.md` surveyed this and
its correction named the gap: native loading of the task-bound subclass and its supporting code.

OR-AG6 asks that OutRig natively load the supplied subclass and application code before
construction, diagnose import and compatibility failure, re-provision on resume, and install
nothing on the model's behalf; artifact forms, identity, readiness milestone and the encoding of
a class reference are open choices. OR-A2-4 fixes the owner: a Rust embedder gives the artifact
through the builder as a path or bytes with an identity, importable in every kernel of the
Session and re-provisioned on resume, with the identity in the Session's facts and the save; the
first application's package has no third-party dependencies. The validation found two precedents
for provisioning (above), that pure Python is a constraint the requirements do not state (its
constraint 1), that the
interpreter program nears its argument cap (constraint 11, `0003-39`), and that "binding" is used
for a package install where the design means the grant of a hosted object (its wording fix).
`application-code.md` records the design. `plan/next/skill-dependency-installation.md` and
`plan/next/pep723-dependencies-for-host-programs.md` are the neighboring optional installs, and
`plan/next/use-the-images-python.md` the only route around pure Python; none changes here.

## Goal

An owner supplies application code -- the task-bound subclass and what it imports -- that OutRig
provisions into the interpreter before any construction, identifies by digest, diagnoses when it
cannot import, and installs the pure-Python dependencies it declares; the model never installs it.

## Deliverables

- **The specification.** Rust: `ApplicationSpec::directory(path)` and
  `ApplicationSpec::archive(bytes)` (OR-A2-4), each with `requires` as a binding's `requires` is
  written, `imports`, the modules imported at start, and an optional `label` of the owner's;
  `SessionBuilder::application(spec)`, once per session. Python: `outrig.Application(path,
  requires=(), imports=())` and `outrig.Session(application=...)`; the owner protocol's
  `session.start` gains `application: {path, requires, imports}`, checked as the program's paths
  are checked. Spellings are this task's.
- **The artifact: a directory, or bytes** (fork 3 for more). A directory is read on the host
  with no Python run, bounded as a skill directory is -- entries, depth, file size, regular files
  only, no link followed; an archive is unpacked under the same bounds as the payload's archive is
  (`unpack`, `payload.rs:343`); either is refused with `not-pure-python` naming the file when a
  `.so` or `.pyd` is in it. The files are copied whole into
  `~/.cache/outrig/applications/<digest>` under the cache root `payload.rs` chooses, staged and
  renamed into place as `write_package` writes the `outrig` package, once per digest; the digest
  over paths and contents, by one recipe recorded in `## Decisions`, is the same for both forms,
  and the label is recorded beside it and compared by nothing.
- **The mount and the line.** The snapshot is mounted read-only at `/outrig/app/<digest>`
  (fork 1), and `requires` is installed with `install::install` into the bindings cache and
  mounted at `/outrig/bindings/<hash>`. Before any kernel opens, the interpreter is told in an
  `application` line: the mount, the digest, the dependency directories and `imports`. It puts
  the paths on `sys.path` at the position fork 2 decides.
- **The import at start** (fork 4). The interpreter imports each module of `imports` on a thread
  of its own -- not the reader thread, not a kernel -- and answers the line per module: imported,
  or `{code, message, traceback}` with the traceback bounded as a skill's `import-failed`
  carries one. The host waits, bounded as it waits for a kernel to report ready, and fails the
  start naming the module and carrying the traceback; a module that does not return within the
  bound fails the start naming it. Codes: `import-failed`, `native-extension-unavailable` when
  `_compiled_cause` verifies compiled code, `module-missing`. A module not in `imports` imports
  when first imported, and a failure then is that import's.
- **Identity.** The digest, the label and the requirement set are recorded in
  `application.provisioned`, are the identity the owner reads in the session's facts, and are
  what `0003-64` saves and compares (OR-A2-4); `agent.instance.started.digest` for a class in the
  provisioned code is its module file's SHA-256, unchanged. The code is importable in every
  kernel of the session, the one the owner constructs its root in included (`0003-46`), since
  the mount and `sys.path` are the interpreter's.
- **The first application's package has no third-party dependency** (OR-A2-4); the `requires`
  path is built and tested here for the applications after it, with a local index as
  `install.rs`'s tests use one.
- **Nothing for the model to install.** `pip` in a kernel is untouched; the orientation says
  nothing new, since the application's modules import as a workspace's do.
- **Events and the close table.** `application.provisioned` -- `path` as given, `digest`,
  `mount`, `requires`, `packages` (the dependency directory, or `null`), `imports` -- and
  `application.imported` per module -- `module`, `code`, `reason`, the last two `null` on
  success -- both execution diagnostics in `doc/reference/events.md`. No close row: the mounts end
  with the container, which **The interpreter and the container** covers, and nothing of an
  application runs outside a kernel after the start; the task's `## Decisions` says so.
- **`SECURITY.md`**, "Known boundaries", gains the entry `application-code.md`, "What application
  code grants", states: the code runs in the container with the agent's authority, is the
  owner's, is read-only for identity and not for protection, and holds no credential.
- **Wording.** The task's docs and docstrings use "binding" only for the grant of a hosted
  object; application code is never called one.
- **`## Decisions` records**: forks 1 through 5; the digest recipe; the bound on the import at
  start; and that pure Python is a fixed fact the application plans around.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise; the mock model is not called in any of them.

- A directory with `review/__init__.py`, `review/agents.py` declaring an `outrig.Agent` subclass,
  and `review/data/notes.txt`: the session starts; in a kernel `import review.agents` works,
  `review.agents.__file__` is under `/outrig/app/`, `importlib.resources.files("review") /
  "data" / "notes.txt"` reads the file, and writing under the mount raises `OSError`;
  `application.provisioned` carries the digest and `application.imported` for `review.agents`
  with `code` `null`. A second session with the same directory copies nothing and records the
  same digest; a directory with one byte changed records another.
- The same files as a tar the test builds and passes as `ApplicationSpec::archive(bytes)`: the
  session starts, `import review.agents` works, and `application.provisioned` carries the same
  digest as the directory form's; a label given on the spec appears beside it.
- `review/agents.py` importing a name its sibling lacks: the start fails; the error names
  `review.agents` and ends with the traceback's last lines; `application.imported` has `code`
  `import-failed`; no kernel was opened; the shutdown report of the failed start is proven
  stopped.
- A module that sleeps past the bound at import: the start fails naming the module.
- A `.so` under the directory: refused on the host before any copy, `not-pure-python` naming the
  file; a symbolic link under it is refused as a skill source refuses one.
- `requires=["pure-fixture==1.0"]` against a local index as `install.rs`'s tests point pip at
  one: importable in a kernel from `/outrig/bindings/<hash>`; a requirement whose only wheel is
  compiled fails the start with `check_pure`'s reason; a path or URL requirement is refused as a
  binding's is.
- `sys.path` order per fork 2: with the recommendation, a workspace file `review.py` does not
  shadow the application's `review` package, asserted by `review.__file__`.
- Editing the directory after the start changes nothing a kernel imports.
- `outrig.Session(application=outrig.Application("./review", imports=["review.agents"]))` in a
  host program starts; a path that does not exist raises before anything starts; `0003-29`'s
  tests pass unchanged.
- `imports=()`: nothing is imported at start, and `import review.agents` in a kernel works.
- A session with no application is unchanged: every existing test passes.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Placement -- Recommended: a snapshot in a digest-named cache directory, mounted read-only
   and placed on `sys.path`**, the binding-packages precedent: `__file__`, `importlib.resources`
   and tracebacks behave as for an installed package, and no import crosses the pipe. The
   alternative is a `meta_path` finder fetched over the pipe from a host-side source, the skills
   precedent, which fits an owner whose code lives in a service and needs no mount, costs a
   request per import, and needs the finder generalized past `outrig_skills`. What decides: the
   first application is a directory on the owner's host; a source form can join later.
2. **`sys.path` position -- Recommended: after `/outrig/lib` and `/outrig/rpyc`, before the
   workspace**, the application's dependency directories with it, so the agent-writable workspace
   cannot shadow the owner's code. The alternative places both as a binding's packages are
   placed, after the workspace, keeping one rule for every path OutRig adds; a workspace module
   named like the application's package would then win.
3. **Artifact forms -- a directory and an archive of bytes now; a wheel open.** A Rust owner
   that ships one executable carries its package as bytes (OR-A2-4); an owner with a checkout
   points at a directory. A wheel file would be checked as `check_pure` checks one and unpacked
   into the cache; it waits for an owner that ships one.
4. **The import at start -- Recommended: on a thread of its own in the interpreter, outside
   every kernel**, with output to the process's stderr, the recorded unattributed bucket. The
   alternative imports only at construction (`0003-46`), which leaves a bad module undetected
   until an owner constructs and gives a session with nothing to construct no check at all.
5. **Snapshot or live mount -- Recommended: snapshot.** A live mount of the owner's directory
   would let the digest drift under the session and the agent see edits mid-round; iteration is
   a new session.

## Decisions

- **Forks 1-5 take their recommendations.** A snapshot in a digest-named cache directory,
  mounted read-only at `/outrig/app/<digest>` and placed on `sys.path` (1); after `/outrig/lib`
  and `/outrig/rpyc` and before the workspace, the requirement set's directory with it (2); a
  directory and an uncompressed tar of bytes now, a wheel open (3); the import at start on a
  thread of the interpreter's own, named `application`, outside every kernel, its output the
  process's stderr and so `output.unattributed` (4); a snapshot, never a live mount (5). The
  `sys.path` move is a move, not a skip: a requirement set the application shares with a binding
  was placed after the workspace by `_bindings`, and goes before it with the code.
- **The digest recipe** is `package_name`'s, split out as `payload::digest`: BLAKE3 over each
  file's path and contents, each preceded by its length as a little-endian `u64` -- a skill
  listing's framing -- in path order, cut to its first 16 hex digits. `package_name` followed
  each with a NUL, which review found ambiguous once contents are arbitrary: `a.txt` holding
  `x\0b.txt\0y` hashed as `a.txt` and `b.txt`, and the second session would reuse the first's
  snapshot. The `outrig` package's directory is renamed by the change, as by any release. It
  names the cache directory and the mount and is what `application.provisioned`,
  `ApplicationIdentity` and a save carry. 64 bits suffice: the owner supplies both sides of every
  comparison, so nothing adversarial chooses a collision. Paths are sorted byte-wise after the
  walk, since the walk's order is not (`/` sorts after `-`). Empty directories are not part of
  it, and a directory and a tar of the same files agree.
- **Reading.** A directory is read by `skills::read_tree`, the walk `DirectorySource` uses
  (`listed`, `read_beneath`), so the bounds, the names skipped (`.`-prefixed, `__pycache__`) and
  the refusal of links and non-regular files are a skill directory's by construction. An archive
  is unpacked with `Entry::unpack_in` into a stage under the cache root -- a member landing
  outside it is refused, as `payload::unpack` refuses one -- and the stage read by the same
  `read_tree`, so an archive reads as the directory does rather than by a second reader's copy of
  the rules. Each member is first put to `skills::admits`, beside the rules it applies: one
  beneath a skipped name -- a `.git`, a `.venv` -- is not unpacked at all, and one past a file's
  size, the depth or the entry count is refused from its header, so the stage never holds what
  the directory form would not have read. Its bounds are never stricter than `read_tree`'s, which
  still decides: entries are counted once per path, as a tar may repeat a directory's header
  before each of its files, and `git archive`'s pax global header is not counted. Every file is
  read whole into memory before anything is written: the digest covers contents, so a warm start
  reads them anyway, and the copy written on a miss is then the bytes that were hashed.
  `ApplicationSpec::archive` takes `Cow<'static, [u8]>`, so an `include_bytes!` static is not
  copied. A skill file's size refusal now reads "a file this source serves", as its siblings do.
- **Pure Python is a fixed fact the application plans around**, not a check the requirements ask
  for: the interpreter is a static build that loads no extension module. `install::extension_module`
  is the one Rust predicate for a compiled file, shared with `check_pure`; a `.so` or `.pyd` in
  the artifact is refused as `not-pure-python` naming the file before anything is written.
  `plan/next/use-the-images-python.md` stays the only way around it.
- **The line is `{"t": "application", paths, imports}` under a request id**, answered by one
  `imported` line per module, `{module, error}`, `error` being `{code, message, traceback}` or
  `null`. The deliverable's line also named the digest; nothing in the interpreter reads it, so it
  is not sent. `paths` is the snapshot's directory then the requirement set's, as the interpreter
  sees them. The host's query is the one that takes several answers: `Query::Imported` holds an
  `mpsc` sender, dispatch finds it with `get`, and `Interpreter::provision` removes it once the
  last module answered, failed or ran out of time. One request per module was weighed and not
  taken: the line the deliverable describes carries the list, and the stream already names the
  module that ran past the bound.
- **The bound** is `READY_TIMEOUT`, 30 seconds, per module, as a kernel's ready is bounded. A
  `#[cfg(test)]` builder hook, `import_bound`, shortens it for the test of a module that sleeps.
  The traceback is the last 16 KiB of `format_exception`, `_serve.TEXT_MAX`, as an event carries
  one; a skill's `import-failed` carries none to copy. A failure's code and message come from
  `skills._import_error`, which a skill's failed import reports through too; `module-missing` --
  a `ModuleNotFoundError` naming the module or a package holding it -- only when that found no
  compiled code, since a module that is a `.so` of another platform's suffix is not found either.
  Formatting the traceback reads the exception, so what that raises (a `__notes__` raising
  `SystemExit`) is reported as `_format_error` reports it, through the shared `_unformatted`, and
  the module is still answered for. The interpreter stops at the first module that did not
  import.
- **A failure** is `InterpreterError::Import { module, reason }`, shaped as `Open { agent, reason }`
  is: "the application's module `review.agents` did not import (import-failed): ValueError was
  raised" followed by the traceback's tail, or "did not import within 30s". Provisioning runs in
  `assemble`, shared by `start` and `start_on_host`, before the primary is built, through
  `Session::provision`; a failure takes `Session::abandon`, the path a primary that cannot be
  built takes, so no kernel was opened and no model called. `Progress::Finished(Step::Python)`
  is reported before the imports, as it is before the primary's kernel opens.
- **"The session's facts" is `Session::application()`**, beside `id()` and `python_version()`: an
  `ApplicationIdentity { digest, label, requires }`, `requires` the sorted, deduplicated set
  `install::requirement_set` makes. Nothing in the crate read "facts" before this task. The host
  program reads the identity from `application.provisioned` in `session.events()`; its
  `session.start` answer is unchanged.
- **`SessionBuilder::application` replaces**, as `container`, `approver` and `repo_root` do; only
  `bind` and `skill_source`, which take many, accumulate. `application::check` -- each
  requirement a specifier, each import a dotted name -- runs in `check()` and at the top of both
  start paths, beside `skills::check_sources`.
- **Mounts.** `package_mounts` in `builder.rs` assembles `spec.packages` once: every requirement
  set's directory, the bindings' and the application's, deduplicated; the snapshot; RPyC.
  `same_paths` keeps its one job, the workspace and mounts at their host paths, and runs only
  with bindings, as before. The read-only mount under `/outrig/app` was run in a container
  (`application_tests.rs`'s e2e test): the reserved-path check applies to caller mounts only.
- **No `label` on the Python `Application`.** OR-A2-4's identity label is the Rust owner's; a host
  program can add one when it needs it. `outrig.Application(path, requires=(), imports=())` is a
  frozen dataclass like `Mount`, its path made absolute when it is made; `prepare` refuses a path
  that does not exist or is not a directory before anything starts, worded as the `cwd` check is.
- **No close row.** The mounts end with the container, which **The interpreter and the
  container** covers, and nothing of an application runs outside a kernel after the start: the
  import thread has ended by the time the start answers.
- **Tests write the user's cache**, as `package_dir` does: snapshots are content-addressed and
  idempotent. "A second session copies nothing" is `a_snapshot_already_taken_is_not_copied_again`,
  which re-provisions into a cache root it cannot write. The acceptance's `pure-fixture` is the
  test wheels' `pure`. `review.agents.__file__` under `/outrig/app/` and the read-only write are
  asserted in the container test; on the host the snapshot is the cache directory itself.
- **Not adopted from review:** moving the bounded reader out of `skills.rs` into a module of its
  own (two callers, one shared walk); one install path for the bindings' and the application's
  sets (three lines alike); preparing the application concurrently with the bindings' installs
  (worth it only for a cold cache, at the price of cross-cleanup). Snapshots are never pruned:
  `plan/next/application-snapshots-are-never-pruned.md`.

## Dependencies

- **Hard: `0003-39`.** The `application` line, its thread and the `sys.path` placement are
  interpreter code.
- **Hard: `0003-20`.** The install path, the pure check and the bindings cache this reuses.
- **Hard: `0003-29`.** The host program's session and the owner protocol's `session.start`.
- **Soft: `0003-32`.** The skills' bounds, codes and finder, which this follows and does not
  change.

## See also

- `plan/phase/0003-python/application-code.md` -- all of it.
- `plan/phase/0003-python/task-agents.md` -- "The owner-constructed root", which imports
  from this.
- `plan/phase/0003-python/hosted-objects.md` -- "Packages"; `plan/phase/0003-python/skills.md` --
  "Sources", "A skill is a module", "Dependencies are checked, never installed".
- `plan/phase/0003-python/discovery.md` -- "Capability is a contract, not a discovery".
- `plan/phase/0003-python/security.md` -- the three Pythons, and what the container grants.
- `plan/next/skill-dependency-installation.md`,
  `plan/next/pep723-dependencies-for-host-programs.md`, `plan/next/use-the-images-python.md`,
  `plan/next/hosted-reference-and-payload-bounds.md`.
- `crates/outrig/src/python/install.rs`, `payload.rs`, `outrig/_interpreter.py` (`_bindings`,
  `_open_imports`, `main`), `crates/outrig/src/harness/bindings.rs`, `skills.rs`,
  `crates/outrig/src/owner/session.rs`, `connection.rs`, `crates/outrig/src/python/outrig/_host.py`.
