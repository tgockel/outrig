# Ordinary child requests can register after the close sweep

## Context

Review of #1070 reproduced this unchanged on `137bdb86`: an ordinary child-channel request
can pass `_refuse_closing()`, pause during `_Work` construction, and register after `_close()`
has snapshotted the registry. The close sweep misses it, leaving a pending handle after
`close_admission()`. This is separate from child-question termination and asker release.

## Shape

- Recheck session closure atomically with registration of an ordinary child request.
- If closing won, settle or refuse the unsent request without publication, and return the
  child-request slot reserved by `child.take()`.
- Audit the equivalent submission/held-instance admission paths for the same ordering.

## Acceptance

- A deterministic gate between request construction and registration lets close win: no
  request survives the close, no false delivery is published, and reserved capacity is returned.
- A request whose registration wins is included in the close sweep and settled exactly once.
