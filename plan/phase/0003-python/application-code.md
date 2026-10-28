# Application code

**Application code** is the package an owner supplies and OutRig provisions into the kernel
before construction: the task-bound subclass and what it imports (the phase README,
"Vocabulary"). This page decides how an owner supplies it, where it lands in the interpreter,
what it may be made of, how it is identified, how a failure to import is diagnosed, how its
dependencies are declared and installed, and what a resume provisions again. `0003-43` builds
all of it; `0003-46` constructs from it (`task-agents.md`, "The owner-constructed root");
`0003-64` re-provisions on resume (`continuation.md`). `0003-39`, the enabling task, moves the
interpreter program itself into the mounted package, which is where this page begins.

One word to fix first. The requirement speaks of "binding-only installation" as a way code might
reach a session (OR-AG6). In the design a **binding** is the grant of a hosted object
(`hosted-objects.md`, "A binding"; the 0003 README's vocabulary), and this page uses the word in
that sense only. Application code is not a binding: it runs in the container, in the agent's
kernel, as the agent's own Python does, and a binding whose factory holds the task-bound class
would put the class in the bindings process on the host, where no kernel's `self` can be. Nor is
it a skill, which is a module the main agent invokes and a source fetches on use (`skills.md`), a
mount the owner manages by hand, or anything the model installs.

## What the mounted package carries

Every session mounts OutRig's own Python package read-only at `/outrig/lib`
(`crates/outrig/src/python/payload.rs:59`), written once into a cache directory named for a
digest of its files (`package_dir`, `:162-174`; `package_name`, `:178-187`; `write_package`,
`:192-209`) and listed in `PACKAGE` (`:63-70`). The interpreter program itself does not travel
that way today: `build.rs` compresses `interpreter.py`, base64-encodes it and stages it as a
one-line `-c` argument (`crates/outrig/build.rs:321-356`), and `Interpreter::start` runs
`python3 -I -c <program> /outrig/lib [/outrig/rpyc]` (`crates/outrig/src/python/host.rs:124`,
`:133`, `:680-697`). Linux caps one argument at 128 KiB (`ARG_STRLEN_MAX`, `build.rs:331`), the
build fails past it, and the program stood at about 118 KB after `0003-34` (its `## Decisions`;
the `plan/next/` entry on the argument cap, folded into `0003-39`). Every task of this phase adds
interpreter code -- the metaclass, facility references, `self`, the construct line, external
endpoints, the MCP presenter, preparation and designation -- so the cap would be reached within
the first tasks.

`0003-39` ships the program in the mounted package, as `outrig/_interpreter.py`, and starts it by
path: `python3 -I /outrig/lib/outrig/_interpreter.py /outrig/lib [/outrig/rpyc]`. `-I` implies
`-P`, so the program's directory is not put on `sys.path`, and `/outrig/lib` joins it where
`_load_outrig` puts it today (`crates/outrig/src/python/interpreter.py:2426-2447`, `:6725-6744`).
The module refuses import under its package name, since it is a program and not a module. The
digest of the package then covers the program, so a kernel always runs the interpreter its build
was made with. `binding.py` and `program.py`, the other two `-c` programs, run on the host where
the package directory is a plain path; whether they move too is `0003-39`'s fork.

## Supplying it

An owner supplies application code when it builds the session, through the surface it already
uses for what it gives a session: `SessionBuilder::bind` and `skill_source`
(`crates/outrig/src/harness/builder.rs:225`, `:278`) for a Rust owner, and `outrig.Session`'s
keywords for a host program (`crates/outrig/src/python/outrig/_host.py:703`), whose `session.start`
request carries them to the engine (`crates/outrig/src/owner/mod.rs:19-24`;
`crates/outrig/src/owner/connection.rs:130-137`). `0003-43` adds an application specification to
both: where the code is, what it requires, and which modules to import at start. One application
per session; a second is the application's own package structure, not a second specification.

The specification names an **artifact**, supplied from Rust as a path or as bytes (OR-A2-4).
The path form is a directory on the owner's host whose top-level entries are packages and
modules, as a project's `src/` is: the owner that holds a checkout of its application points at
it. The bytes form is an archive of the same layout the embedder carries in its binary --
`include_bytes!` of a tar it built -- for an owner that ships one executable and no directory
beside it; it is unpacked into the cache as the payload's archive is unpacked (`unpack`,
`payload.rs:343`), and from then on the two forms are one. A wheel file is a natural third form,
since the pure-Python check for one exists (`check_pure`,
`crates/outrig/src/python/install.rs:353-395`), and is open. Either form is read on the host
with no Python run, bounded as a skill directory is bounded -- entries, depth, file size, regular
files only, no link followed (`DirectorySource`, `crates/outrig/src/harness/skills.rs:120-133`)
-- and copied whole into a cache directory named for its digest, staged and renamed into place
as the `outrig` package is written (`payload.rs:192-209`). The session mounts that snapshot, so
the agent sees one version for the session's life, an edit the owner makes meanwhile changes
nothing until the next session, and a resume can tell whether the code it is given is the code
the save was made under ("Identity"). The code is importable in every kernel of the session,
since the mount and `sys.path` are the interpreter's and not a kernel's.

## Where it lands

The snapshot is mounted read-only at `/outrig/app/<digest>`, beside `/outrig/bindings/<hash>`
(`payload.rs:49`), and the interpreter is told of it in an `application` line before any kernel
opens, as it is told the bindings and the skills (`_bindings`, `interpreter.py:6176-6204`;
`declare_skills`, `host.rs:950-953`). The line names the mount, the digest, the dependency
directories and the modules to import at start.

Where it goes on `sys.path` is `0003-43`'s fork. Recommended: after `/outrig/lib` and
`/outrig/rpyc` and before the workspace, so the owner's code cannot be shadowed by a file in the
agent-writable workspace, and its declared dependencies go with it. The alternative places it as
a binding's packages are placed, after the workspace and before the agent's own installs
(`_bindings`, `interpreter.py:6198-6202`), keeping one rule for every path OutRig adds; a
workspace module named like the application's package would then win, which is the ordinary
`sys.path` hazard `discovery.md` chose to avoid for the standard library.

Two provisioning mechanisms exist, and the fork between them is the requirement's open choice of
artifact form. The read-only mount placed on `sys.path` is how a binding's packages reach the
container (`hosted-objects.md`, "Packages"), and is recommended: `__file__`,
`importlib.resources` and tracebacks behave as for any installed package, and nothing crosses the
pipe at import. A `meta_path` finder fetching files over the pipe from a host-side source is how
skills reach it (`skills.md`, "Sources"; `_Finder`,
`crates/outrig/src/python/outrig/skills.py:830-859`), and fits an owner whose code lives in a
service rather than a directory; it costs a request per
import and a generalization of a finder that serves one namespace today. An owner with such a
catalog is not the first application, and the mount form can be joined by a source form later
without changing what a kernel sees.

The agent's own `pip install` is untouched (`discovery.md`, "Capability is a contract, not a
discovery"); it installs into the agent's environment, after everything OutRig placed.

## Pure Python only

The interpreter is OutRig's static CPython, which loads no third-party extension module
(`discovery.md`, "Capability is a contract, not a discovery"), and so is the bindings process
(`hosted-objects.md`, "Where it runs"). Application code and every module it imports in a kernel
must be pure Python; a dependency is either vendored in the artifact or declared and installed
by the wheels-only, `py3-none-any` path ("Dependencies"). The artifact check refuses a `.so` or
`.pyd` under the directory on the host, before anything is copied or mounted, with the reason
`check_pure` gives (`install.rs:73-76`). The requirement says "no full arbitrary dependency
manager" and does not say this; it is a fixed fact of the architecture, and the one recorded way
around it -- running the interpreter on the image's own Python, `plan/next/use-the-images-python.md`
-- is an opt-in that loses the memory ceiling's coverage of native allocations and does not reach
the bindings process.

## Identity

An artifact's identity is the digest of its files' paths and contents, as `package_name`
computes the `outrig` package's (`payload.rs:178-187`), the same for a directory and for bytes
that unpack to the same files. It names the cache directory and the mount, is carried by the
`application.provisioned` event, and is what a save records as the code the session ran
(`continuation.md`). The owner may attach a label of its own -- its release's version, say --
which is recorded beside the digest and compared by no one: the digest is the identity, and the
label is for the owner reading the facts (OR-A2-4; `rust-owner.md`). A class's own digest is
unchanged: `agent.instance.started` carries a SHA-256 of the class's module file (`_digest`,
`_agents.py:432-453`; `doc/reference/events.md`), which for a provisioned module is the file
under the mount.

## Diagnostics

Nothing of an application fails for the first time in a model round. The specification names the
modules to import at start -- the package that holds the task-bound class, typically -- and the
interpreter imports each as the session starts, on a thread of its own: not the reader thread,
which never runs agent code or waits (`_open`, `interpreter.py:6249-6254`), and not a kernel,
since a session with no primary has none until a root is constructed. The import runs outside
every execution, so `outrig.runtime` raises in it as it raises anywhere outside one
(`_kernel.py:17-23`), which a module should not touch at import, and its output goes to the
process's stderr, the unattributed bucket the host records (`agent-placement.md`, "Output stays
attributed"). The interpreter answers the `application` line per module: imported, or failed with
a code and a bounded traceback. The host waits, bounded as it waits for a kernel to report ready
(`READY_TIMEOUT`, `host.rs:141`), and a failure fails the session's start naming the module and
carrying the traceback; no kernel was opened and no model called.

The codes are the skills' (`skills.md`, "Dependencies are checked, never installed"):
`import-failed` for a module that raised, with its traceback; `native-extension-unavailable` when
`0003-10`'s diagnostic verifies compiled code (`_compiled_cause`, `interpreter.py:1447`);
`module-missing` for a name the artifact does not hold. A module the specification does not name
imports when something first imports it -- the construct of a class in it (`0003-46`), or agent
code -- and a failure then is that import's, before any round the construction would have had.

## Dependencies

Application code declares what it requires the way a binding does: requirement specifiers, a
name with extras, a version specifier and markers, never a path or a URL
(`crates/outrig/src/config/binding.rs`, `requires`; `hosted-objects.md`, "Packages"). The set is
installed by the same path, once, into the same cache keyed by the set (`install::install`,
`install.rs:98-118`; `bindings_dir`, `:80-82`): wheels only, with pip's source-distribution step
disabled, every wheel checked pure (`check_pure`), and the directory mounted read-only at
`/outrig/bindings/<hash>` as a binding's is (`crates/outrig/src/harness/bindings.rs:200-225`,
`:452-454`). The owner is trusted as the operator is, so the declaration needs no approval, as
`SessionBuilder::bind` needs none (`builder.rs:207-209`). What the agent's environment then has
is what the owner declared and nothing the model chose; the model never installs application
code or its dependencies (OR-AG6). The first application's package has no third-party dependency
(OR-A2-4): its facilities are services in Rust, and its Python imports the standard library and
`outrig`. The dependency path exists for the applications after it.

Two neighboring entries stay what they are. `plan/next/skill-dependency-installation.md` and
`plan/next/pep723-dependencies-for-host-programs.md` ask for optional, off-by-default installs of
what a skill or a host program declares in a PEP 723 block, because a file in a repository or a
script cannot make a grant by itself; an owner's application specification is the owner's grant,
so the install here is neither optional nor approved.
`plan/next/hosted-reference-and-payload-bounds.md` bounds what a hosted connection holds and is
unchanged: an application's pure-Python client of a
service is hosted as any library is (`embedding.md`, "Services written in Rust").

## Re-provisioning on resume

A save holds no code: "Import runs no source. A conversation's executions are a record, not a
script" (`continuation.md`, "The format"), and `lifecycle.md` has the rule for any
continuation -- a fresh interpreter, a notice, no replay. What a save holds is the application's
identity: the artifact's digest, the requirement set, and the classes constructed from it. On
resume the owner supplies application code as for any session, OutRig provisions it the same way,
and compares: the same digest resumes silently; a different one is reported -- in the
`application.provisioned` event, in the reconciliation notice the model reads, and to the owner
-- rather than substituted silently. Whether a mismatch refuses the resume or proceeds with the
notice is `0003-64`'s fork; this page fixes that it is never silent. Release versioning and
cutover policy are deferred by the requirements and decided nowhere here.

## What application code grants

Application code runs in the container with the agent's authority, which is arbitrary execution
there (`security.md`, "OutRig's existing property, unchanged"). It is the owner's code, trusted as
the owner is; it is mounted read-only so the artifact the session ran is the artifact the owner
supplied, which is what the digest attests, and not so that the agent cannot change its behavior:
the agent's code and the application's share one heap, and a monkeypatch is plain Python.
Anything the process can read, the model can print (`security.md`, "OutRig's existing property,
unchanged"), so application code holds no credential: a client that needs one is a binding or an
MCP placement on the trusted side (`security.md`, "The trusted side runs on the host";
`mcp-presenter.md`), and the application passes the reference. `SECURITY.md`'s "Known boundaries"
gains this entry with `0003-43`.

## Events

`0003-43` adds two execution diagnostics: `application.provisioned` -- the path as the owner gave
it, the digest, the mount, the requirement set and its directory, and the modules to import --
and `application.imported`, one per module imported at start: `module`, and `code` and `reason`,
`null` unless the import failed. No close row: the mounts end with the container (`lifecycle.md`,
"The close table", **The interpreter and the container**), and nothing of an application runs
outside a kernel after the start. `0003-46` adds `agent.construct.failed`.

## Rejected alternatives

**Rejected: publishing the application to an index and declaring it as a binding's `requires`.**
A binding's packages reach the container (`hosted-objects.md`, "Packages"), but a binding exists
to host an object on the host, and the requirement is explicit that "Binding-only installation,
PyPI publication, manual import paths and skills are not substitutes" (OR-AG6).

**Rejected: skills as the vehicle.** A skill is a module the main agent invokes through a
directive, fetched on use; a task-bound class is constructed by OutRig before any round and
imported by name. The skills' finder is a precedent for the fetched form, not the mechanism.

**Rejected: a mount the owner manages, with the owner's own `sys.path` edits.** The code would
reach the container with no identity, no pure-Python check, no import diagnostic and no
re-provisioning, which is the gap `python-package-deployment-assessment.md` found and the
requirement closes.

**Rejected: the model installing the application.** It would make the application's presence
depend on a model round, and `discovery.md` already says a failed install is the worst discovery
path.

**Rejected: mounting the owner's directory live.** The digest would drift under the session, a
resume could not say what code the save was made under, and the agent would see edits mid-round.

**Rejected: cloning modules per agent.** `sys.modules` is the interpreter's; two agents share
one module and keep separate instances (`task-agents.md`).

## Open questions

- Whether `run-new` gains a way to supply application code for the primary. The CLI's primary is
  OutRig's own agent, the phase's exit criteria give the CLI save and resume only, and a
  task-bound primary under the CLI is a later question.
- A wheel as a second artifact form, and a source form for an owner whose code lives in a
  service.
- Whether several applications per session are ever needed, or one package with several classes
  is always the shape.
- Whether the modules imported at start should default to every top-level package in the
  artifact rather than to what the owner names.

## Unverified

- `-I` implying `-P` (no script directory on `sys.path`) is Python 3.11 behavior read from the
  documentation; `0003-39` asserts it on the pinned payload.
- The cost of importing an application at start on a thread of its own was not measured; the
  start already waits for the interpreter's greeting and each kernel's ready within 30 seconds.
- That a read-only mount under `/outrig/app` is accepted by the reserved-path check
  (`reject_reserved`, `payload.rs:241`) was read, not run: the check refuses caller mounts under
  `/outrig`, and OutRig's own mounts are made beside it.
- Whether blake3 over paths and contents (`package_name`'s recipe) or SHA-256 (the class
  digest's) is used for the artifact is `0003-43`'s to choose; this page says only that one digest
  names the artifact everywhere it is recorded.
