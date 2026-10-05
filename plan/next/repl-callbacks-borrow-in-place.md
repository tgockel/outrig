# `Repl::run`'s callbacks could borrow their state instead of moving it

`Repl::run` bounds its callbacks on `FnMut(String) -> impl Future`. That bound cannot lend a
borrow of the callback's state to the future it returns, which is why `outrig run`'s loop moves
the conversation out with `mem::take` for the length of a turn and `run-new` keeps its agent
behind a `Mutex`. 0.2.1 fixed the loss that arrangement caused -- Ctrl-C mid-turn dropped the
future and the history with it -- by putting what was moved out back on drop.

Bounding the callbacks on `AsyncFnMut` instead would let both borrow in place: the future
borrows the state for as long as it runs, and a cancelled future takes nothing with it, so there
is nothing to restore. The restore-on-drop guard then goes, and `run-new`'s `Mutex` has one
reason fewer to exist.

## Why deferred

The restore-on-drop fix is correct and tested, and this is a change to `Repl::run`'s signature
that touches every caller. It wants doing when the REPL is next opened up, not on its own.
