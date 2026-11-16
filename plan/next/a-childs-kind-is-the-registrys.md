# A child's kind is the registry's, and the runner still threads it

## Context

`0003-49` gave each `ChildNode` its `ChildKind`, which the roster reads to tell a work child from a
request child (`crates/outrig/src/agent/children.rs`). The runner already had the kind: `runner::run`
takes it as a parameter and passes it back on every question it asks the registry --
`Inner::summary(child, kind)` each time it decides whether to open a round, and the `quiet`
closure `make` builds over `Inner::quiet(child, kind)`. So the kind now lives in two places,
set from the same spawn line, and nothing keeps them apart but that they were set once each.

Nothing is wrong at runtime: a child's kind never changes. The cost is to readers, who see the
kind handed back to the registry that holds it, and to the next change that adds a question
about a child, which has to choose which copy to read.

Found by `/simplify` on `0003-49`. Left alone there because the change is in the runner and
`make`, which that task does not otherwise touch.

## Sketch

- `State::summary` and `Inner::quiet` read `ChildNode::kind` themselves and drop their `kind`
  parameter; `Inner::summary` and `quiet`'s closure follow.
- `runner::run` keeps `kind` only for what it does itself -- how a round's opening reads, and
  what the warning and the cancel call what they end -- or asks the registry for it once at its
  start.
- `State::summary`'s unit tests build their child through `children.rs`'s `child` fixture, which
  sets the kind, rather than passing it.
