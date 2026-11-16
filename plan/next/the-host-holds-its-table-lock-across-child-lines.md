# The host holds its table's lock while the registry handles a child line

## Symptom

`dispatch` (`crates/outrig/src/python/host.rs`) hands a line about child agents to the
registry as `match lock(table).children.clone() { Some(children) => children(&agent, value),
.. }`: a temporary in a match scrutinee lives to the end of the match, so the table's guard is
held across the callback. A handler that answers the kernel (`Interpreter::tell`) or reads the
session's admission (`Lifecycle::closed`, through the gate) takes the same lock and hangs.
`agent.spawn`, `agent.release` and `0003-50`'s `agent.model_requests` run on tasks of their own
for that reason, `tell_life` explains it, and the first build of `agent.model_requests` hung on
it. Every new handler has to remember. Found by `0003-50`'s altitude review.

## Shape

Bind the clone to a local and drop the guard before calling the handler, as the `agent.gone`
path does. Before that, check that no handler relies on the lock for ordering against the
table's other users; then the per-kind task hop for answering can go, and so can the comment in
`tell_life`.
