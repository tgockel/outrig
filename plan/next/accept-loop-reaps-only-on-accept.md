# The accept loop reaps on accept, not as connections complete

## Problem

`accept_into`'s doc comment says the claim it exists to make observable is "that finished
connections are taken back out of the set as they complete" -- but the loop
(`crates/outrig/src/network.rs`) only ever reaps on its way in to a `select!`:

```rust
loop {
    reap_finished(conns);
    tokio::select! {
        _ = cancel.cancelled() => break,
        accepted = listener.accept() => { /* ... conns.spawn(...) ... */ }
    }
}
```

So a handle is taken back when the *next connection arrives*, not when its connection ends. An
attachment that serves a burst and then goes quiet holds one `JoinSet` entry per connection in that
burst until something else connects or the attachment is torn down -- which is the accumulation
`reap_finished` exists to prevent, just on a longer timescale. There is no reap after `break`
either; `tcp_accept_loop`'s drain is what collects those.

Nothing is leaked in the sense of a live task: these are finished tasks whose handles are still
held. It is bounded by "connections served since the last accept", which for a busy attachment is
small and for a bursty one is the whole burst.

## Sketch

Give the `select!` a third branch and move the `spawn` out of it, so the mutable borrow of `conns`
ends with the select expression rather than crossing the arm that spawns:

```rust
let accepted = tokio::select! {
    _ = cancel.cancelled() => break,
    Some(_) = conns.join_next() => continue,
    accepted = listener.accept() => accepted,
};
match accepted { /* as today */ }
```

`JoinSet::join_next` is cancel-safe, so losing the race to `accept` costs nothing. On an empty set
it yields `None`, and a branch whose pattern does not match is disabled for that `select!` rather
than spinning -- the other two still park. `reap_finished` stays: it collects the rest of a batch in
one pass instead of one select wake per handle.

Worth checking while doing it: `Some(_)` swallows a connection's `JoinError` exactly as
`try_join_next` does today, which is the subject of `connection-panics-are-not-reported.md` -- the
two want deciding together rather than twice.

## What it would let the test say

`the_accept_loop_keeps_taking_finished_connections_back` currently asserts `conns.len() < 8`,
because the connection served last is still held whatever happens. With the loop draining while it
is parked, the residue is only what is *still running* at the cancel, so the bound could become
something the test names exactly rather than a slack figure.

## See also

- `plan/next/connection-panics-are-not-reported.md` -- same reap path, and the same `JoinError`.
