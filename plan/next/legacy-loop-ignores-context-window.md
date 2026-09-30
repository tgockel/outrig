# `outrig run` parses `context-window` and ignores it

## Symptom

`0003-12` added `[models.<name>].context-window` for `outrig run-new`, whose loop holds each model
call to it. The key lives on `Model`, which both loops share, so `outrig run` and `run-legacy`
accept it -- and send whatever their conversation has grown to, as before. A user who sets it for
one command and runs the other gets no word that it did nothing.

## Why it was left

`crate-split-tradeoffs.md` rules out editing `outrig-cli`'s loop during phase 0003: the 0.2.x line
keeps changing its own copy, and a merge from it has to apply without conflict. The legacy loop
has no store and no view, so honoring the key there is a budget built from nothing, not a port.

## Shape

- Cheapest: `run` warns once at startup when the resolved model sets `context-window`, saying only
  `run-new` reads it. Touches `outrig-cli`'s startup, not its loop.
- Otherwise nothing until `run` itself is retargeted at the Python loop, when the question goes
  away.

`doc/reference/config.md` already says `run` ignores the key.
