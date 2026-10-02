# An execution's events name the tool call that asked for it

## Context

`events.jsonl` says which executions ran in a round, and the round's turns say which tool calls
the model made, but nothing joins the two. `exec.submitted` carries an `execid` and the source;
the tool call in `turn.committed` carries rig's call `id` and the same source. So
`scripts/render-session.py` (`0003-14`) links a call to an execution only when their sources
match exactly within the round, first come first served. Two identical submissions in one round
can be linked crosswise, and the renderer cannot tell.

`exec.refused` is thinner still: an `execid` and a `holder`, and no source at all. A refused
submission's code is only in the turn that asked for it, which the renderer finds by elimination.

This is the causal-parent question `observability.md` leaves open ("which model turn produced
which execution"), and the answer is narrower than a general parent field: the tool call id is
already in hand where the tool runs.

## Shape

- `exec.submitted` and `exec.refused` gain `call`: the tool call's id, as rig gives it to the
  tool. `exec.refused` also gains `source`.
- The renderer links by `call` when it is there and falls back to the source match for older
  logs, which it has to keep reading.
- `doc/reference/events.md` loses the note that a refused submission's source is only in its turn.

## Acceptance

- A round that submits the same source twice links each call to its own execution, in the
  renderer's tests and in an `events_tests.rs` assertion that the ids match.
- A refused submission's source is on its execution's entry in the page.
