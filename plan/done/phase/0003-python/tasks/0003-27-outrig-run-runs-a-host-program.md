# 0003-27 -- `outrig run` runs a Python program on the host

## Context

The host-program requirements (R1, reviewed 2026-10-14) add a second way to drive OutRig: a
trusted Python program on the host, run by OutRig's bundled CPython, that decides for itself when
to enter a session and run agents. Launching the program starts no container and calls no model.
This task is the launcher and the program's view of the configuration; `0003-29` gives the
program a session, and `0003-35` and `0003-36` give it agents and live objects.

What exists: `payload::host_python()` is the bundled interpreter on the host
(`crates/outrig/src/python/payload.rs`); `supervisor::start` launches a supervised child in a
process group of its own with a parent-death signal, from a thread that lives as long as the owner
(`crates/outrig/src/python/supervisor.rs`); config discovery from the invocation's directory is
`repo_cmd_ctx` and `resolve_repo_config_optional` (`crates/outrig-cli/src/paths.rs`); the `outrig`
package the program imports is `0003-26`'s, whose source lives beside the payload for the kernel
and the host alike. What does not: `run`'s `RunArgs` has flags and no positional, `run-new`'s the
same; `binding.py`'s entry point moves the protocol descriptors, redirects fd 1 and installs a
signal handler, so it cannot bootstrap a program whose streams must stay its own.

The requirement spells the command `outrig run <script.py> [args...]`. In this phase `run` is still
the legacy loop, with `run-legacy` as its alias, and `run-new` the new system's interactive
command; retargeting `run` is release-phase work. So a positional script makes `run` dispatch to
the new engine, a bare `outrig run` stays the legacy loop, and the retarget later folds the two.
Everything after the script belongs to the program, `--help` included.

This is trusted host execution, not a sandbox. The program reads host files, holds the user's
environment and runs host commands; `-I` governs how the interpreter starts and what it imports,
not what the program may do. Trusted imports and credentials belong outside paths an agent can
write.

## Goal

`outrig run worker.py ./workspace "Fix the tests"` runs `worker.py` on the host as an ordinary
program -- its `__main__`, its argv, its cwd, its streams and its exit status -- with
`import outrig` available and the configuration loaded, and starts no container and calls no model
until the program asks.

## Deliverables

- **The command.** `outrig run <script.py> [args...]`: a positional script and trailing arguments
  (`trailing_var_arg`, hyphen values allowed), everything after the script passed through as the
  program's argv; OutRig's options go before it. With a script, every `RunArgs` flag given before
  it -- `--agent`, `--model`, `--image`, `--session-dir`, `--max-tool-calls`,
  `--max-tool-result-bytes`, `--env`, `--network`, `--volume` -- is refused with an error naming
  the flag, since the program's session chooses its own inputs, and the same spelling after the
  script is the program's argument; the global `--config`, `--global-config`, `--session-root`
  and `-v` apply before the script. A bare `outrig run` is the legacy loop as before,
  `run-legacy` and `run-new` are unchanged, and `run`'s help says which form does what and names
  the retarget as later work.
- **The launch.** `payload::host_python()` with `-I` and a bootstrap program given as `-c`. The
  bootstrap takes the owner connection's descriptor numbers as its own argument (fork 3); puts the
  host `outrig` package first after the standard library and the script's directory last on
  `sys.path`, with no cwd, no `PYTHONPATH` and no user site; sets `sys.argv = [script, *args]`
  with the script as given; and runs the file as `__main__` through
  `runpy.run_path(path, run_name="__main__")`, so `__file__` is the script and `__name__` is
  `"__main__"`. The cwd is the invocation's. The bootstrap runs no asyncio loop on the main thread;
  the host package serves its connection on a thread of its own, so `asyncio.run` in the program
  works.
- **Streams.** stdin, stdout and stderr are inherited, and OutRig never reads or parses them. The
  owner connection is two inherited pipe descriptors marked `O_CLOEXEC`, so a subprocess the
  program starts holds neither, and the engine learns of the program's exit from the process, not
  from a grandchild letting go. Launcher diagnostics go to stderr.
- **Supervision.** The program is a supervised child as the bindings process is: its own process
  group, the parent-death signal, spawned from the supervisor's thread. Its exit status passes
  through -- `sys.exit(2)` is 2, falling off the end is 0, an uncaught exception prints Python's
  traceback and is 1 -- and a death by signal reports per fork 1. A failure before the program
  runs -- a missing or unreadable script, a payload that cannot be unpacked, a configuration that
  does not load -- is a launcher diagnostic and status 1, before any session resource exists.
- **Signals.** SIGINT and SIGTERM received by `outrig run` with a script are forwarded to the
  program per fork 2, and that is all this task does with them: no session can be open yet, so
  there is nothing to clean. `0003-29` adds the cooperative grace and the escalation.
- **Configuration.** Discovered from the invocation's directory through `repo_cmd_ctx`, with
  `--config` and the global file as for every command, loaded and validated once before the
  program runs, and never rediscovered from the script's directory or a later workspace. The
  loaded `Config` is what `0003-29` builds the program's sessions from.
- **`outrig.config()`** (R7): a read-only view sent to the program at bootstrap, as frozen
  dataclasses per fork 4: the concrete models by name, each with its provider, its effective
  identifier -- the configured one, or the row's name where validation allows its absence -- its
  `context-window` and `max-tokens`; the aliases by name, each with its candidates in order; the
  providers by name, each with `style` and `base-url`; `default-model`; the configured workspace
  host path, resolved, or `None`; and `[events] mode`. No API key, no `${VAR}` value and no other
  secret is in it, and no kernel's `outrig` ever carries it. `help(outrig.config)` documents the
  view and says that a declared window is a configured number, not the model's capacity.
- `doc/reference/cli.md` and `doc/usage/run.md` describe the script form as a preview beside the
  legacy form, with the retarget named as later work.

## Acceptance

- **The requirement's example runs on the host alone.** `outrig run worker.py ./workspace "Fix
  the tests"` with the two-line program from R1: no container is created (`podman ps -a` is
  unchanged), the mock provider receives no request, the two lines print, and the status is 0.
  The same program returning 2 exits 2; one that raises exits 1 with Python's traceback on
  stderr; a missing script exits 1 with a launcher diagnostic and starts nothing.
- **Argv is the program's.** `sys.argv` is `[script, *args]`; an argument with spaces, an empty
  argument and `--help` after the script arrive unchanged; `outrig run --help` is OutRig's help;
  `__name__` is `"__main__"`, `__file__` is the script, and `os.getcwd()` is the invocation's
  directory.
- **Imports.** A script outside the cwd imports a module beside it; `PYTHONPATH` and the user site
  are ignored; the cwd is not on `sys.path`; `import outrig` works and `import sys; sys.path[0]`
  is the standard library's, not the script's.
- **Streams carry no protocol.** A program that writes binary junk to stdout, closes fd 1 and
  reopens it, and reads a line from stdin leaves the owner connection working, asserted by
  `outrig.config()` answering afterward. A child `subprocess.Popen(["sleep", "60"])` holds
  neither owner descriptor (`/proc/<pid>/fd`), and the engine sees the program's exit while that
  child lives.
- `asyncio.run(main())` works, and a second `asyncio.run` after the first does too.
- **Configuration comes from the invocation.** With one repo config in the cwd and another in the
  script's directory, `outrig.config()` shows the cwd's; `--config` names a third and wins.
- `outrig run --agent x script.py` is refused naming `--agent`, while
  `outrig run script.py --agent x` runs the script with `--agent x` in its `sys.argv`; bare
  `outrig run`, `outrig run-legacy` and `outrig run-new` behave as their existing tests assert.
- **Signals reach the program.** SIGINT and SIGTERM to `outrig run` each reach a program with
  handlers, which prints and exits 0; a program without handlers exits by the signal with fork
  1's status.
- **The view.** `outrig.config()` lists each configured model with its effective identifier and
  window, each alias with its candidates in order, each provider's style and base URL, and holds
  no key and no `${VAR}` value; `import outrig; outrig.config` is absent in a kernel.
- `crates/outrig/public-api.txt` regenerated only if the library gains a public item;
  `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Exit status when the program dies of a signal -- Recommended: 128 plus the signal number**,
   the shell convention, so a service unit reads it as it reads any program's. The alternative is
   1, which a program's own `sys.exit(1)` cannot be told from.
2. **What a forwarded signal is sent to -- Recommended: the program's process group**, which is
   what a terminal's Ctrl-C would have reached had the program been in the foreground group. The
   alternative signals the process alone and leaves its subprocesses to it.
3. **How the owner descriptors are named to the bootstrap -- Recommended: as the bootstrap's own
   argument, removed before the program sees `sys.argv`**, so neither argv nor the environment
   the program and its children see carries protocol detail. The alternative is an environment
   variable, which every subprocess inherits.
4. **The shape of the configuration view -- Recommended: frozen dataclasses with docstrings**, so
   `help()` answers and an absent field is `None` rather than a missing key. The alternative is
   nested dicts mirroring the TOML.

## Dependencies

- **Hard: 0003-18.** The supervisor: the process group, the parent-death signal, the spawning
  thread.
- **Hard: 0003-26.** The `outrig` package whose source the host imports.

## See also

- `plan/phase/0003-python/embedding.md` -- "A Python program as owner".
- `plan/phase/0003-python/harness-components.md` -- the program's process in the picture.
- `plan/phase/0003-python/security.md` -- the program as trusted host code.
- `crates/outrig/src/python/supervisor.rs`, `payload.rs` and `binding.py` -- what is reused and
  what cannot be.
- `crates/outrig-cli/src/cli/run.rs`, `app.rs` and `paths.rs` -- the command and the discovery.

## Decisions

All four forks were taken as recommended: a death by signal is 128 plus its number, a forwarded
signal goes to the program's process group, the descriptors are the bootstrap's own arguments,
and the view is frozen dataclasses.

- **The terminal stays with OutRig, and a program does not read it.** In its own process group,
  the program is a background group on a terminal. On this host, a child in its own group that
  read the terminal was stopped by SIGTTIN (state `T`), which would have hung `outrig run`. The
  acceptance's pipes would never have shown it. Handing the program the foreground with
  `tcsetpgrp` was weighed and declined. The keyboard's Ctrl-C would then reach the program and
  never OutRig, so `0003-29`'s grace and escalation could not start from it, and Ctrl-Z would
  have needed shell-style job control. Sharing OutRig's group would lose the group that cleanup
  and escalation act on. The maintainer's ruling is that a host program does not call `input` or
  `getpass`. The pre-exec hook therefore ignores SIGTTIN and SIGTTOU, so a read from the terminal
  fails at once with `EIO` (`input()` raises `EOFError`) instead of stopping the program. What
  the program starts inherits both ignored dispositions. Input piped or redirected into
  `outrig run` reads as usual. No `plan/next/` entry was filed for terminal input.
- **`outrig.config()` asks the engine on every call.** The view is computed once, from the
  `Config` the launcher loaded, when the program starts. The program fetches it with a request
  over the connection, so each call that answers is also proof that the connection works. That
  is the acceptance's check after junk on stdout. The id-matched request and reply, served by a
  reader thread in the program and a tokio task in the engine, is what `0003-29`'s session calls
  build on. Each call returns new frozen dataclasses with `MappingProxyType` mappings.
- **An alias's candidates are flattened** by `Config::model_candidates`: the concrete rows in the
  order a call tries them, an alias of aliases expanded and duplicates dropped. The engine
  resolves names, so what a program needs is what will be tried. A configuration that was never
  validated and whose alias cannot be walked shows the targets it declares. `identifier` falls
  back to the row's name. `workspace` is the declared host path, or `None` when nothing declares
  one, resolved by `resolved_host_path` as the mount and validation resolve it. `..` is kept,
  for the filesystem to follow. Review found that the lexical form first used here
  (`known_host_path`) could name another directory: with `link` a symlink to `actual/child`,
  `link/../ws` is mounted as `actual/ws`, while the lexical form gave `ws` beside `link`. The
  view reads only `models`, `providers`, `default-model`, `[workspace]` and `[events]`, so a
  literal in an image's build arguments or a server's `env` cannot reach it.
- **A workspace whose path is not UTF-8 refuses the program.** Linux allows a directory such as
  `repo-\xff`, and validation accepts a workspace under one, but the view is JSON, whose strings
  hold text. Review found that `json!` unwrapped the failed serialization, so `outrig run`
  panicked with status 101 before the script ran. Replacing the bytes would name another
  directory, and sending them raw for Python to decode with `os.fsdecode` would give the view's
  one path field an encoding of its own. So `view` is fallible: `HostProgram::start` returns a
  configuration error naming the path, before anything starts, and `outrig run` prints it with
  status 1. A configuration that declares no workspace runs in the same directory.
- **In a kernel, `outrig.config` is an `AttributeError`, not a `RuntimeError`.** `0003-26` made
  `runtime` and `bindings` raise `RuntimeError` on the host, so that they would not read as
  typos. Here the acceptance asks for `config` to be absent, so that `hasattr` answers. The
  message still says that it exists only in a program `outrig run` runs on the host.
  `schema_tests` already held `hasattr(outrig, 'config')` false on a bare host interpreter.
- **`run-legacy` is a subcommand of its own, no longer an alias.** As an alias it would have
  taken the script form along with `run`'s positional. As its own subcommand over the same
  `RunArgs` it stays exactly the interactive session, which is what it has to mean once `run` is
  retargeted. `run_legacy.rs` no longer holds `run --help` equal to `run-legacy --help`, or `run`'s
  help free of the word Python: both were promises from the alias era. It now holds that the two
  take the same session flags, that only `run` takes a script, and that `run`'s help names both
  forms and the retarget.
- **The refusal is clap's.** The positional is `trailing_var_arg` with
  `conflicts_with_all = SESSION_FLAGS`, the ids of `RunArgs`' flags. A conflict with `RunArgs`'
  derived group would have listed every flag in the group; listing the ids names only the flag
  given. A unit test holds the list equal to `RunArgs`' arguments, and the refusal is a usage
  error with status 2. `allow_hyphen_values` is not set. Without it, an unknown flag before the
  script is still an error instead of becoming the script's name, a script whose name starts
  with `-` follows `--`, and everything after the script reaches the program unchanged: `--`,
  `-v`, `--config` and `--help` included.
- **A script must be a regular file that this process can open.** `runpy.run_path` would run a
  directory's `__main__.py`, but it would put the directory first on `sys.path`, against the
  import rule, so a directory is refused. The check runs before the payload is unpacked. The
  crate-private `Spec` gained a `cwd`, `None` in production, so a relative script is checked
  where the program will find it.
- **The script runs in a `__main__` of its own, which stays registered.** `runpy.run_path` puts
  the bootstrap's module back in `sys.modules` when the script's body returns, which is before
  the exit handlers run, so an `atexit` callback found none of the script's names in `__main__`
  and could not pickle its classes. Review found this; `python3 <script>` keeps the script's
  module to the end. The bootstrap builds the module as `run_path` does -- `__file__`, and
  `None` for `__cached__`, `__loader__`, `__package__` and `__spec__` -- registers it for the
  interpreter's life, and runs the script's code in it. It compiles the code from source, or
  reads a compiled file with `pkgutil.read_code`, as `runpy` does.
- **A traceback starts at the script.** Without trimming, every traceback opened with the whole
  `-c` line, then the bootstrap's frames. The bootstrap wraps `sys.excepthook` to
  cut the leading frames that are not the script's. It sets the exception's `__traceback__` as
  well, because since Python 3.12 the default hook prints the exception's own traceback. A syntax
  error shows no frames, as `python3 bad.py` shows none. A program that replaces
  `sys.excepthook` gets the untrimmed traceback. Exceptions still leave through the bootstrap, so
  Python's own exit handling applies: `SystemExit`'s status, 1 after the traceback, and death by
  SIGINT for an uncaught `KeyboardInterrupt`.
- **The program ends with the engine through two detectors, as the bindings process does.**
  - The parent-death signal, SIGHUP. The program's `_die` handles it on the main thread: SIGTERM
    to the group, which the program itself ignores, a one-second grace, then SIGKILL to the
    group.
  - EOF on the connection, which the reader thread sees. The thread raises SIGHUP in the process
    so that `_die` runs. After the grace and one more second it sends SIGKILL to the group
    itself. That covers native code holding the main thread, and a SIGHUP handler of the
    program's own.

  A diagnostic on the way is written on a thread of its own and never waited for. Stderr is the
  program's, a pipe the engine neither owns nor drains, and review found that a full one held
  `_die` in its first line, before any signal: `outrig` killed, its program and the program's
  children lived on. A read error on the connection breaks it without killing anything: it means
  the program closed or replaced the descriptor, not that the engine is gone. A forked child
  closes its copies and puts SIGHUP back to its default. The engine holds a duplicate of its write
  end, so the serving task ending never looks like the engine's death.
- **What a program leaves running after a normal exit keeps running.** `wait` reaps the program
  and returns, as with `python3 prog.py` in a shell. Dropping a `RunningProgram` before then kills
  the program's group. No signal is sent after the reap, when the group's id could name another
  group.
- **The library launches and forwards; the CLI owns the signal handlers.** `HostProgram`
  installs no handler, because a library that took over SIGINT would change its embedder's
  process. The CLI registers SIGINT and SIGTERM only once the program has started, so a Ctrl-C
  during the first payload unpack ends `outrig` by the default action, with nothing to clean up.
  An `outrig` that starts with SIGINT ignored, as a background job of a non-interactive shell
  does, hands its program that ignored SIGINT, as for any child.
- **`supervisor.rs` shares its mechanics, and the handle that keeps them.** The bindings
  process and the program are started the same way:
  - `supervise` sets the process group, the blocked parent-death signal and the parent check;
  - `spawn_supervised` spawns from the one parent thread, renamed `outrig-parent`;
  - the test-only probe rides in a `Supervision` value.

  `spawn_supervised` returns a `Supervised` handle that owns the child and its group's rules:
  - an owner's signal reaches the group only while the child is unreaped, since tokio forgets
    the id of a child it has reaped;
  - the group is killed when the handle is dropped before it is settled;
  - `stop`'s escalation lives on the handle, moved there from the bindings `Process`.

  Settled means the group is no longer the handle's to end: `stop` proved it empty, or reaped the
  child after its last `SIGKILL`; `kill` reaped the child; or `wait` saw the program exit on its
  own, which leaves the rest of its group running by design. That is the old `Process` flag,
  with the program's normal exit kept apart. Review found that the first version of the handle
  keyed the drop on the leader's reap alone. A shutdown cancelled after `stop`'s polling had
  reaped a leader that SIGTERM ended then left a child that ignores SIGTERM running.

  `stop` still reaps the leader during the grace, as the bindings stop always has. Peeking with
  `WNOWAIT` instead would keep the leader as a zombie member, and the group could not be proven
  empty without reading `/proc` on every poll. The drop of a cancelled stop signals a group
  whose survivors still hold its id; the window in which the group empties and its id is reused
  is the one the bindings stop already had.

  Before this, the bindings `Process` and the program's `Program` each rebuilt those rules, and
  the program's start took the child out of the handle before steps that could still fail.
  `0003-29` can stop a program with the sequence that stops a bindings process. The program adds
  a pre-exec closure for the terminal signals, and `bootstrap` builds its command line -- argv
  and the descriptors kept across exec -- for `start` and for the test that closes the engine's
  end by hand.
- **`outrig` still imports eagerly on the host.** About 26 ms of a program's start, out of
  roughly 66 ms in all, is `outrig.schema` and `dataclasses`. A lazy `schema` alone saves about
  3 ms, because `_host` defines its dataclasses at import, and the rest would need the view's
  types in a module loaded on first use. That was declined: a host program exists to use
  `outrig`, and once sessions and agent classes arrive it imports `schema` either way, so
  deferring only helps a program that never touches the package.
- **The CLI tests reuse the user's payload cache.** A fresh `XDG_CACHE_HOME` per run would unpack
  about 170 MB each time. The library's tests already share `~/.cache` in the same way. Each test
  passes an absent `--global-config` and its own working directory. `podman` and `buildah` are
  stubs that log every call, which is how "no container is created" is asserted without a
  runtime. Ctrl-C at a real terminal is exercised under a pty: `openpty`, then `setsid` and
  `TIOCSCTTY` in the child, which adds nix's `term` and `process` features as a dev-dependency.
- **The script form moved to `run-new` (#662).** The decision above to spell it on `run` held that
  `run` is retargeted at 0.3.0, so the script form would already be at its final name. That covers
  the state after the retarget. Until then it put a program on the command of the legacy loop, which
  only starts containers, with a positional that refused every session flag `run` has and a
  `--grace` that `run` refused without one, all in one `--help`. The retarget does not need it
  there: when `run-new` becomes `run`, the script form moves with it. So `--grace` and the
  positional are `RunNewArgs`', the refused list names `run-new`'s four session flags, `--agent`,
  `--model`, `--image` and `--session-dir`, and `Cmd::Run` is `Run(RunArgs)` again, as on 0.2.x. A
  script given to `run` or `run-legacy` is still clap's usage error, status 2, and names `run-new`:
  `RunArgs` carries a hidden trailing positional, an `OsString` because `run-new` runs a script
  whose path is not UTF-8, whose value parser refuses with that message, so the words after the
  script are absorbed rather than reported as unexpected, and `run --help` shows neither it nor a
  Python program. Nothing had shipped under the old spelling, so the changelog entries were
  rewritten rather than amended.
