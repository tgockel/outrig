# A request-count test can count a release before the worker lets go of the proxy

## Context

`python::binding_tests::an_awaited_path_makes_a_request_per_step` (`0003-21`) measures what each
awaited path sends, releases included: for `await stub.send(1)`, one `getattr`, one `call`, and
one `del` for the bound method's proxy. Its `measure` drops the result, collects, and sends one
more request, which flushes the release queued by the collection.

The path is resolved on a worker thread of the kernel's pool, and the awaiting task's future is
settled from that thread before the worker's own frame lets go of the proxies it resolved. When
the kernel's thread runs the collection in that window, the method's proxy is still referenced,
no release is queued, and the count reads `dels: 0`. It is flushed with the next measurement's
first request, which that measurement clears.

It was seen three times during `0003-22`, each in a full parallel test run on a loaded machine
-- `send: ... left: Number(0) right: 1` -- and never in 48 concurrent runs of the test alone,
before or after `0003-22`'s changes.

## Options, none chosen

1. **The worker drops what it resolved before settling the future**, so a proxy the path no
   longer needs is released by the time the awaiting task resumes. That changes the facade, and
   makes every release prompt, not only the test's.
2. **The test waits for the worker**: `measure` waits until the pool has no busy worker before it
   collects.
