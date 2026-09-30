# REPL: interrupting a turn silently empties conversation history

`run_repl`'s `on_prompt` moves the shared history out of its `RefCell`
(`std::mem::take`) so the borrow isn't held across the `run_turn` await, then
writes it back after the turn. The comment says cancellation "may add partial
history to `h`, so it must always be written back" -- but when SIGINT cancels
the in-flight callback, the REPL's `tokio::select!` drops the future before
the writeback line runs. The taken vec (all prior turns plus the partial
turn) is dropped, and the shared slot is left holding the empty vec that
`mem::take` put there. The next prompt silently starts from blank history.

Pre-existing behavior, observed during the 0002-07 dispatcher refactor and
deliberately preserved there.

## Sketch

Restore on drop instead of after the await -- a small guard owning the taken
vec that writes back in `Drop` (covering both completion and cancellation),
or switch the history slot to something the callback can mutate in place
without holding a borrow across the await.

A deeper form of the same fix: bound `Repl::run`'s callbacks on `AsyncFnMut` rather than
`FnMut(String) -> impl Future`. The current bound cannot lend a borrow of the callback's state to
the future it returns, which is why `run` moves the history out and `run-new` (`0003-05`) keeps
its agent behind a `Mutex`. With `AsyncFnMut` both could borrow in place, and a cancelled future
would take nothing with it.

## Acceptance

- A repl_io-style test: prompt turn interrupted mid-callback, then a
  follow-up turn observes the prior history rather than an empty one (needs
  a hook to observe what `on_prompt` receives, or a run.rs-level test around
  the closure).

## `run-new`'s loop does not have it

`0003-11` gave `PythonAgent` a store that owns the conversation, so a round borrows nothing it
could take down with it. rig is handed only the round, each turn is committed as it completes,
and a dropped round's guard commits the turn it was in. Interrupting a `run-new` round -- while
the model is called or while its Python runs -- leaves everything before the round, and what the
round finished. `agent_tests.rs`'s `run_new_keeps_the_conversation_when_a_round_is_interrupted`
pins both.

`run` and `run-legacy` still take the `mem::take` path in `outrig-cli`'s `run_repl`, and still
lose the conversation, so this entry stays open for them.
