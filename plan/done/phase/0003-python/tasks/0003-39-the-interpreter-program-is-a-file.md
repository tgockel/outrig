# 0003-39 -- The interpreter program is a file

## Context

The interpreter program, `crates/outrig/src/python/interpreter.py`, reaches the container as a
`-c` argument: `build.rs` reads it, compresses it, base64-encodes it and writes a one-line
bootstrap that inflates, compiles and executes it (`PROGRAMS` and `programs`,
`crates/outrig/build.rs:321-356`), asserting the result is under `ARG_STRLEN_MAX`, the 128 KiB
Linux allows one argument (`:331`, `:346-351`). `host.rs` includes the bootstrap as `PROGRAM` and
starts the payload's `python3` with `["-I", "-c", PROGRAM]` followed by `/outrig/lib` and, with
bindings, `/outrig/rpyc` (`crates/outrig/src/python/host.rs:124`, `:133`, `:680-697`). The tests
start it the same way on the host (`crates/outrig/src/python/testing.rs:191`, `:262`). `main()`
checks `len(sys.argv)` is 2 or 3 and loads OutRig's package from `sys.argv[1]`
(`interpreter.py:6725-6744`).

OutRig's own package already travels as a mount: `PACKAGE` lists its files
(`crates/outrig/src/python/payload.rs:63-70`), `package_dir` writes them once into a directory
named for their digest (`:162-174`, `package_name` `:178-187`, `write_package` `:192-209`), every
session mounts it read-only at `/outrig/lib` (`PACKAGE_MOUNT`, `:59`; `mounts`, `:229-234`), and a
test holds that every file under `src/python/outrig/` is in the list (`:636-652`).

`0003-34`'s `## Decisions` records the program at about 118 KB of the cap, from 113 KB;
the `plan/next/` entry on the argument cap recorded the growth and the two shapes of a fix,
and this task absorbs it. The validation of the phase's requirements found that every
cluster adds interpreter code -- the metaclass and the hooks (`0003-41`, `0003-47`), facility
references and the view (`0003-42`), `self` (`0003-45`), the construct line (`0003-46`), the
`application` line (`0003-43`), external endpoints, the MCP presenter, preparation and
designation -- and would hit the cap within the first tasks (its constraint 11). This is the
enabling task, with no requirement ID of its own; `application-code.md`, "What the mounted
package carries", records the decision.

## Goal

The interpreter program reaches the container in the mounted `outrig` package and is started by
path, so the `-c` argument cap no longer bounds the interpreter's code.

## Deliverables

- `interpreter.py` moves to `crates/outrig/src/python/outrig/_interpreter.py`, is listed in
  `PACKAGE`, is written by `write_package` and is part of `package_name()`'s digest, so a kernel
  runs the interpreter its build was made with. Its module docstring says it is the interpreter
  program, run as `__main__` by path, and the module refuses import under its package name:
  `import outrig._interpreter` raises `ImportError` saying the program is not a module, so no
  second copy of its classes is ever made.
- `Interpreter::start` runs `python3 -I /outrig/lib/outrig/_interpreter.py /outrig/lib
  [/outrig/rpyc]`. `ARGS` loses `-c` and `PROGRAM`; `host.rs`'s `PROGRAM` constant and the
  `("interpreter", ...)` entry of `PROGRAMS` go; the two starts in `testing.rs` run the file from
  `package_dir()` on the host. `main()`'s argument check is unchanged, since `sys.argv[0]` is the
  path where it was `-c`.
- `-I` implies `-P`, so the program's directory, `/outrig/lib/outrig`, is not on `sys.path`;
  `/outrig/lib` joins it where `_load_outrig` puts it today. A test asserts both.
- A failure inside the program names the file's path in its traceback, where it named
  `<interpreter>` before.
- `binding.py` and `program.py` stay `-c` bootstraps, with `ARG_STRLEN_MAX`'s assert over them
  (fork 2).
- `plan/next/interpreter-program-nears-the-argument-cap.md` is folded into this task, which is
  its fix; the entry is removed.
- `doc/reference/events.md` gains nothing: the start's `interpreter.*` events are unchanged.
  `lifecycle.md`'s close table gains no row: no new resource. The task's `## Decisions` says both.
- `crates/outrig/CHANGELOG.md`'s `[Unreleased]` notes the move; `crates/outrig/public-api.txt`
  regenerated, with no change expected.

## Acceptance

- `cargo build` no longer asserts the interpreter program's size; the asserts over `binding` and
  `program` stay and pass.
- A session starts with the program from the mount: the interpreter greets, and the interrupt,
  runaway, memory-ceiling, user-channel, hosted-object, skills and children suites pass
  unchanged, started through `testing.rs`.
- In a kernel, `sys.path` holds `/outrig/lib` and not `/outrig/lib/outrig`; `import
  outrig._interpreter` from agent code raises `ImportError` with the message, and `outrig`'s
  other modules import as before.
- `package_name()` changes when the program's source changes: a test writes the package with one
  byte of the program altered and sees another directory name.
- An exception raised in the program's own code -- a test that corrupts a protocol line the
  reader handles -- reports a traceback naming `/outrig/lib/outrig/_interpreter.py` on the host.
- The bindings process still starts from `binding.bootstrap` and the host program from
  `program.bootstrap`; `0003-27`'s and `0003-20`'s tests pass unchanged.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Where the file lives -- Recommended: `outrig/_interpreter.py`, inside the package.** One
   mount, one digest, and the test that every file under `src/python/outrig/` is listed holds.
   The alternative, a file at the mount's root as `/outrig/lib/interpreter.py`, would be
   importable as `interpreter` from every kernel, since `/outrig/lib` is on `sys.path`, and would
   shadow a workspace module of that name.
2. **Whether `binding.py` and `program.py` move too -- Open; recommended: no.** Both run on the
   host, where the package directory is a plain path, and neither is reported near the cap.
   `0003-36` reshapes `binding.py` into a wrapper over a module of the package, which is the
   moment to move what remains of it.
3. **Run by path, or by a one-line `-c` bootstrap that `exec`s the file -- Recommended: by
   path.** `__main__.__file__` is set, tracebacks name the path, and no decompression runs at
   start. The alternative keeps the `-c` shape for no gain.

## Dependencies

- **Hard:** none in this phase. The landed `0003` line is the program this moves; `0003-34` is
  the task whose `## Decisions` records its size.
- **Soft:** none.

## See also

- `plan/next/interpreter-program-nears-the-argument-cap.md` -- the entry this task absorbs.
- `plan/phase/0003-python/application-code.md` -- "What the mounted package carries".
- `plan/phase/0003-python/agent-placement.md` -- the interpreter process and its kernels.
- `plan/phase/0003-python/discovery.md` -- the `sys.path` order the program keeps.
- `crates/outrig/build.rs`, `crates/outrig/src/python/host.rs`, `payload.rs`, `testing.rs`.

## Decisions

- **Fork 1 -- inside the package, as `outrig/_interpreter.py`.** As recommended: one mount, one
  digest, and `the_package_lists_every_file_in_its_directory` holds with the file listed.
  `payload::PROGRAM` names its path under the package directory; `Interpreter::start` joins it to
  `PACKAGE_MOUNT`, and `testing.rs` to the host's `package_dir()`.
- **Fork 2 -- `binding.py` and `program.py` stay `-c` bootstraps.** At the move their bootstraps
  were 6,170 and 2,098 bytes against the 131,072 the cap allows, where the interpreter's was
  124,986. `ARG_STRLEN_MAX`'s assert stays over both.
- **Fork 3 -- run by path, with `__file__` dropped.** Run by path, `__main__.__file__` names the
  program, and Python 3.13's `multiprocessing` runs a `__main__` that names its file again, as
  `__mp_main__`, in every child its `spawn` and `forkserver` methods start
  (`multiprocessing/spawn.py`: `get_preparation_data`, `_fixup_main_from_path`). The program's
  body moves fds 0 and 1 and patches `sys.stdout`, `threading` and `os.system` as it runs; under
  the import guard the child dies instead. `pip_installs_a_pure_package_that_imports_at_once`
  starts a `spawn` child, and without the fix that child exits 1. So the program deletes
  `__file__` straight after its guard, and a child leaves `__main__` alone, as it did under `-c`;
  `a_spawn_or_forkserver_child_runs_none_of_the_program` starts one of each and fails without it.
  Of fork 3's reasons this gives up "`__main__.__file__` is set", which nothing read. Tracebacks
  still name the path, since they read the code objects' filename, and nothing is decompressed at
  start.
- **The guard refuses every name but `__main__`**, not only `outrig._interpreter`: `runpy` under
  any other name would make the second copy of the program's classes the guard exists to
  prevent. With `__file__` gone, `multiprocessing` never runs the program as `__mp_main__`. Its
  message names the name it was run under, and the program's last line calls `main()` with no
  `if __name__ == "__main__"` around it, since the guard has made that test already.
- **The traceback acceptance runs through a line the reader routes.** `_handle` reports a line it
  cannot route in one diagnostic, `ignored a message: <error>`, with no traceback, so no line it
  rejects names a file, under `-c` or now. The test corrupts the answer to a spawn instead: the
  reader hands a `spawned` that names no child to the spawn waiting on it, `_spawn` raises
  `KeyError` in the program's own code, and the execution's traceback names
  `<package>/outrig/_interpreter.py` (`a_failure_in_the_programs_own_code_names_its_file`). The
  e2e test `the_program_runs_from_the_mounted_package` checks the literal
  `/outrig/lib/outrig/_interpreter.py` through `runtime.nothing`, which `Runtime.__getattr__`
  refuses in the program's own code. `_handle`'s diagnostic is unchanged.
- **"`outrig`'s other modules import as before" has no test of its own.** Every start imports
  `outrig._agents`, `outrig._kernel`, `outrig.schema` and `outrig.skills` from the package
  directory before it greets, so every interpreter test covers it; importing them again in a
  test would be answered from `sys.modules` and could not fail.
- **The program's frames quote their source.** A frame of the program below the agent's own in
  an execution's traceback -- inside `runtime.spawn`, say -- now shows its line, as frames of the
  package's other modules already did. `_format_error` still drops the frames above the
  submission's.
- **`package_name` and `write_package` take the file list**, which `package_dir` passes as
  `PACKAGE`, so `a_program_one_byte_different_is_written_under_another_name` can write a package
  whose program differs by one byte.
- **`main()`'s usage line** names `sys.argv[0]`, the program's path, in place of `-c <program>`;
  the argument count it checks is unchanged.
- **Nothing to remove from `plan/next/`.** `interpreter-program-nears-the-argument-cap.md` was
  deleted by the planning round that folded it into this task (`5238383`).
- **No event and no close row.** `doc/reference/events.md` gains nothing: the start's
  `interpreter.*` events are unchanged. `lifecycle.md`'s close table gains no row: the program is
  a file of a mount every session already had, not a new resource.
- **Plan references follow the file.** In `plan/todo/` and `plan/phase/`, a reference to
  `crates/outrig/src/python/interpreter.py` names `crates/outrig/src/python/outrig/_interpreter.py`,
  and a bare `interpreter.py` names `_interpreter.py`, or `outrig/_interpreter.py` in a list of
  paths under `crates/outrig/src/python/`. Line numbers are as they were written. Left alone:
  `plan/done/`; this file's Context and forks, which describe the program before the move; and
  `0003-67`'s fork 5, which reasons from the argument cap this task removes.
