# run-new: a slow terminal slows the round

`converse` (`crates/outrig-cli/src/cli/run_new/converse.rs`) drives the round's future as one
branch of its `select!`. A future is polled only when the loop reaches it, and each of the agent's
messages is written inside its branch, awaiting the terminal. `select!` picks among ready branches
at random (`0003-08` removed `biased`, which starved everything behind a stream of messages). So
behind a terminal slower than the agent, the round is polled on only some iterations, and each one
lasts as long as a write. Its model calls and tool steps then progress at the terminal's pace.

`0003-08` observed this while reproducing a review finding. A round whose code sent 40 messages to
a terminal reading 10 ms a line finished only after every message had been shown -- about 160 ms
after its execution had reported. Nothing is lost and nothing hangs; the round is slow.

## Sketch

Either stop the loop from blocking, or stop the round from depending on it:

- Move terminal writes to a writer task fed by a channel, so each branch returns at once and the
  round is polled every iteration. That also answers `/simplify`'s note that a stalled stdout
  (a paused pipe, Ctrl-S) freezes the round.
- Or run the round as its own task over `Arc<tokio::sync::Mutex<PythonAgent>>`, and abort it
  where the future is dropped today. `KeptIfDropped` and `Unhanded` already cover a round that is
  dropped mid-await, which is what an abort is.

The writer task is the smaller change, and it keeps the one-owner arrangement the module doc
states.

## Acceptance

- With stdout read slowly (as `run_new_e2e.rs`'s `read_slowly` does) and a burst of messages
  queued, a round's reply reaches stderr before the burst has drained.
