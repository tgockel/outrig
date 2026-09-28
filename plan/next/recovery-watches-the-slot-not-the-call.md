# The runaway watch belongs to the slot, not to a call waiting on it

## Context

`0003-06` checks an outstanding execution from inside `recovery::settle`, which runs only while a
tool call waits on that execution. Once nothing waits -- the execution was recorded `Unresolved`
by a second interrupt or three failed ones, or a caller dropped the round -- nothing watches it:

- A spinning abandoned holder burns a CPU between submissions, and at the prompt, indefinitely.
- The only time the host looks at it again is when a submission is refused behind it, and that
  refusal waits for a whole check (`recovery::rescue`) -- up to a 5 s probe and two CPU readings --
  before the model sees it. A model retrying N times waits about 5N seconds.
- `settle`, whose job is to wait on an execution, handles refusals too, and `Stop::Holder` is about
  a different execution than the one its `Settled` reports.

The design review of `0003-06`'s `/simplify` pass named the deeper change. It was left because it
reworks the host's lifecycle late in the task.

## Shape

- The host runs a check task for each slot from the moment it is taken until its reply arrives,
  whether or not anyone waits. It keeps the last verdict, and the count of interrupts that left
  the loop spinning, in `Slot`.
- `settle` shrinks to the outcome and the user's interrupts, reading "gave up" and "interrupted a
  runaway" from the slot. Giving up stops waiting; watching continues.
- A refusal reads the holder's last verdict at once. `rescue` and `Stop::Holder` go.
- The check task must not keep the interpreter's stdin open: a strong `Interpreter` clone in it
  would stop the writer from closing stdin when every handle drops, and the interpreter from
  exiting. A weak sender, or ending the task with the reader, is needed.

## Acceptance

- A refusal behind an abandoned holder returns without waiting on a probe.
- An abandoned holder found spinning is interrupted with no submission needed to prompt it.
- Dropping every handle still ends the interpreter while a check task runs.
