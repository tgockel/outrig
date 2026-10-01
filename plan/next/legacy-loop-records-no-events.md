# `outrig run` parses `[events]` and records nothing

## Symptom

`0003-13` added `[events] mode = "record"`, which has `outrig run-new` write its agent's events to
`<session_dir>/logs/events.jsonl`. `[events]` lives on `Config`, which both loops share, so
`outrig run` and `run-legacy` accept the block and write no file. A user who turns it on and runs
`run` gets no word that nothing was recorded.

## Why it was left

`crate-split-tradeoffs.md` rules out editing `outrig-cli`'s loop during phase 0003. The legacy loop
has no store, no manifest, and no interpreter protocol, so most of the event catalog has no
producer there; recording it would be a second schema, not a port.

## Shape

- Cheapest: `run` warns once at startup when `[events] mode = "record"`, saying only `run-new`
  records. Touches `outrig-cli`'s startup, not its loop.
- Otherwise nothing until `run` is retargeted at the Python loop, when the question goes away.

`doc/reference/config.md` and `doc/reference/events.md` already say `run` records nothing. The
same shape as `legacy-loop-ignores-context-window.md`, and worth doing in one change with it.
