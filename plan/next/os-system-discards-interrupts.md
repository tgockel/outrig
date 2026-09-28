# `os.system` discards an interrupt sent while it waits

## Context

musl's `system()` sets SIGINT to `SIG_IGN` for the whole process while it waits for its child,
as POSIX asks of it. An interrupt the host sends during that wait -- automatic or the user's
Ctrl-C -- is discarded rather than deferred: measured on the payload, `os.system("sleep 2")`
completed and no `KeyboardInterrupt` ever fired. Because the disposition is process-wide, one
agent's `os.system` also swallows an interrupt meant for another's wedge on the main thread.

`0003-06` names this in `runtime-protection.md` and leaves it: the automatic path checks again
later, and the thread measures idle anyway, so only an explicit Ctrl-C is affected. It is also the
one common spawn that bypasses the interpreter's `Popen` patch, so its output is billed to no
execution.

## Shape

Replace `os.system` in the interpreter with a version built on `subprocess.run(cmd, shell=True)`,
returning the wait status `os.system` returns (`returncode` re-encoded). That makes it
interruptible like any `subprocess.run` -- the direct child killed, its descendants not -- and
brings its output under the same attribution as every other child.

## Acceptance

- An interrupt during `os.system("sleep 60")` ends the execution with `KeyboardInterrupt`.
- `os.system("exit 3")` still returns `768`, and its output is billed to its execution.
