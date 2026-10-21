# The session page lays out one agent's rounds

## Context

`scripts/render-session.py` reads a session's events as one agent's timeline: a `round.started`
opens a block, and the next `model.round.completed` closes it. Since `0003-30` a session can hold
child agents, each with rounds of its own, under subjects `agent/c1` and so on, and their rounds
run while the main agent's round runs. The page closes the main agent's open round as unfinished
when a child's begins, and shows `agent.spawned`, `agent.request.*`, `agent.released`,
`agent.wedged`, and `model.round.usage` as raw data, which the sweep in
`scripts/render-session-test.py` accepts because every field still reaches the page escaped.

## Shape

- Rounds are tracked per subject, so a child's round neither closes nor joins its parent's.
- Each child gets a section, linked from the `agent.spawned` that launched it, holding its rounds
  and executions, as the main agent's section holds its own.
- A request's events are gathered on one entry -- sent, received, each answer, how it ended --
  linked from both the sender's and the child's timeline.
- A round's `total` and `descendants` are shown beside its own `usage`, with any
  `model.round.usage` that arrived after it.
