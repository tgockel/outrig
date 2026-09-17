# Connection task panics are not reported

A panicking `handle_tcp` is thrown away rather than surfaced. Both places the accept loop
reaps connections test only `is_some()`:

- `crates/outrig/src/network.rs`, the accept loop's final drain: `while
  conns.join_next().await.is_some() {}`
- `reap_finished`: `while conns.try_join_next().is_some() {}`

Each discards an `Err(JoinError)`, so a connection that panicked never becomes the
`NetworkTaskPanicked` that `stop_tasks` reports for the accept and DNS loops, and `detach`
returns `Ok(())`.

This predates 0002-40 in effect -- on trunk, connections were `tokio::spawn`ed with the handle
dropped, so a connection panic was already lost -- but 0002-40 added a reporting contract for the
supervisor loops that these two sites do not extend to connections.

## What to do

Propagate the `JoinError` from both sites into the attachment's teardown failures, as
`OutrigError::NetworkTaskPanicked`. `reap_finished` currently returns nothing; it would need to
hand back what it collected, or take a sink to push into.

## Why it was not done in 0002-40

Found in review of that task and scoped out: the behavior is unchanged from trunk, and the task's
acceptance is about attach/detach being inverses rather than about panic reporting.
