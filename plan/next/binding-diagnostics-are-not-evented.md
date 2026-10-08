# A binding's own diagnostics reach the log, and no event

## Context

`host.rs` drains the interpreter's stderr line by line and records each line as an event --
`InterpreterDiagnostic` for the program's own `outrig-interpreter:` lines, `OutputUnattributed`
for anything else -- beside logging it, so a session's record shows what the interpreter said.
`0003-18`'s supervisor drains a binding's stderr the same way but only logs it through `tracing`,
keeping the last lines for the error a failed start reports. A binding's own diagnostics -- a line
it ignored, a connection whose serving failed, the group stop it began -- and whatever the hosted
library prints are in the log and nowhere else.

## Shape

- A binding's `outrig-binding:` lines become an event naming the binding, in the category
  `0003-21` records hosted calls in; the library's other output becomes an unattributed-output
  event as the interpreter's does, so a `print` in a hosted library is recorded as what it is.
- The supervisor takes the session's `Events` as the interpreter's host does, once `0003-20` gives
  it a session to belong to.

## Acceptance

- A binding whose factory prints a line, and one that refuses a request so that it writes a
  diagnostic, each leave an event in the stream that names the binding; neither reaches the
  terminal.
