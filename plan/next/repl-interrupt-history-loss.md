# REPL: interrupting a turn silently empties conversation history

`run_repl`'s `on_prompt` moves the shared history out of its `RefCell`
(`std::mem::take`) so the borrow isn't held across the `run_turn` await, then
writes it back after the turn. The comment says cancellation "may add partial
history to `h`, so it must always be written back" -- but when SIGINT cancels
the in-flight callback, the REPL's `tokio::select!` drops the future before
the writeback line runs. The taken vec (all prior turns plus the partial
turn) is dropped, and the shared slot is left holding the empty vec that
`mem::take` put there. The next prompt silently starts from blank history.

Pre-existing behavior, observed during the 0084 dispatcher refactor and
deliberately preserved there.

## Sketch

Restore on drop instead of after the await -- a small guard owning the taken
vec that writes back in `Drop` (covering both completion and cancellation),
or switch the history slot to something the callback can mutate in place
without holding a borrow across the await.

## Acceptance

- A repl_io-style test: prompt turn interrupted mid-callback, then a
  follow-up turn observes the prior history rather than an empty one (needs
  a hook to observe what `on_prompt` receives, or a run.rs-level test around
  the closure).
