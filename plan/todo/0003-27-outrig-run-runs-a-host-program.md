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
