# Two defects found by the agent living in the kernel

Found by running against a fresh kernel in a subprocess; both reproduce deterministically.
Patched kernel: crates/outrig-cli/src/python/kernel.py.patched
Diff:           kernel-findings.diff

## 1. A wedged loop is unrecoverable (severe)

`{"t":"exec","src":"while True: pass"}` blocks the event loop forever. Verified: after that,
inventory never answers, `deliver_user` never arrives, further execs never run, and closing
stdin does NOT free it (the documented clean-exit path only works via the loop). The host has
no timeout anywhere -- `round_trip`'s oneshot simply never resolves, so the turn hangs rather
than erroring. One bad comprehension from the model bricks the agent until the container dies.

The reader thread is fine throughout; every path it has goes through `loop.call_soon_threadsafe`,
which is exactly the queue a wedged loop is not draining.

Fix: a `{"t":"interrupt"}` message, handled ON the reader thread rather than via the loop,
which raises SIGINT; a handler installed from inside `_main` turns it into a KeyboardInterrupt
inside the running execution, and is a no-op when nothing is running. The execution comes back
as a normal error result with a traceback. 12/12 trials: recovered, kernel alive, session usable.

Two rejected alternatives, both empirically:
  - `ctypes.pythonapi.PyThreadState_SetAsyncExc` -- `ctypes.pythonapi` is None in this
    statically linked build. Would have silently done nothing.
  - `_thread.interrupt_main()` -- killed the process in 9 of 10 runs.
A plain SIGINT to the process without the in-loop handler also fails: asyncio.run's own handler
defers it until the loop next runs a callback.

Host side still needs: a bounded wait in `execute()` that sends the interrupt and surfaces
"the execution was interrupted after N seconds" -- otherwise nothing ever sends the message.

## 2. Background output silently evicts the next result

`_capture` bills all output to the current execution's 16 KiB, including bytes written between
executions. A background task printing ~35 KB between turns caused the NEXT execution's output
to be 100% noise -- `print("THE ANSWER I WANTED")` did not appear at all. I hit this by accident
in my own session before testing it deliberately: the result I asked for was simply gone, with
nothing to indicate it had been displaced.

Fix: track `_foreground`; out-of-band output goes to a separate 2 KiB tail-kept deque, reported
as a labelled, byte-counted preamble. Background tracebacks are still visible -- often they are
the whole story -- but they can no longer push out what the model asked for.

## What held up under attack

Protocol/stdout separation (raw `os.write(1,...)` and subprocess stdout both captured, neither
can forge a frame), output bounded at the fd, the `sys.modules` trick (`get_type_hints` works on
model-defined dataclasses), notification-without-consumption, `<execution>` in tracebacks,
unknown message types and unknown channels ignored safely, and the wait/interrupt semantics
exactly as documented. 15/15 invariants pass on the patched kernel.

## One design note, not a bug

`runtime.wait` raising MessageAvailable and leaving the operation alive is the right call, and
it is the thing that makes being interrupted cheap rather than lossy. Suggestion: the
MessageAvailable message could name the variable holding the operation when it is bound in
globals, since the recovery move is always "await it again by name".
