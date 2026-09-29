# Nothing stops an execution that holds the slot while no call waits for it

## Context

`0003-06` made Ctrl-C reach the Python a call is waiting on. Two paths leave an execution holding
the interpreter's slot with *no* call waiting:

- Ctrl-C pressed twice on one execution, which stops waiting for it and records it
  `Unresolved`.
- A round whose future is dropped while a call waits on Python. The REPL no longer does this --
  its Ctrl-C reaches that Python instead -- but any other caller of `PythonAgent::round` can.

From then on `PythonAgent::interrupter` returns `None`, because nothing is waiting, and the only
time the host looks at the holder again is when a submission is refused behind it
(`recovery::rescue`). That check interrupts a holder it finds spinning, and says what it found
otherwise. It never cancels, since the host does not cancel on its own account. So a holder
suspended on an await that never resolves -- one that caught the cancel from the first press,
say -- refuses every later submission for the rest of the session, and the model can only report
it.

Since `0003-09`, a holder suspended in `runtime.wait` is freed by the next line the user types,
which raises `MessageAvailable` in it -- unless its code catches that too. A holder suspended on
a bare `await` is not, and that is the case left here.

## Shape

- A user-facing stop for the held slot. Ctrl-C at the prompt while an execution holds the slot
  could cancel it, and check as the first press does, before falling back to the prompt's
  fresh-line behavior. Or `run-new` could take a `/stop` slash command;
  `plan/next/run-new-flag-parity.md` already notes it has only `/help` and `/quit`.
- `PythonAgent` needs a second entry point for it, or `interrupter` could reach the abandoned
  holder when no call waits (`Interpreter::abandoned` already names it). The latter changes what
  `None` means to the caller, which today drops the round on it.

## Acceptance

- An execution that catches its cancellation and is abandoned by a second press can still be
  stopped from the prompt, and the next submission then runs.
