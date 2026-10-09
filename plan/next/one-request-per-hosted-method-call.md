# A hosted method call costs two requests

## Context

`0003-17` counted the requests each operation of a service-shaped binding makes. A method call on
a proxy -- `root.ask("...")` -- is two round trips and a release: RPyC's
`BaseNetref.__getattribute__` sends a `getattr` for every name outside its local set, so the bound
method arrives as a proxy of its own, the call is a `call` on that proxy, and the method proxy's
finalizer queues a `del`. The
`callattr` request, which does the lookup and the call in one, is sent only by the methods
`class_factory` generates on the proxy's class -- and Python reaches those only for the special
methods it looks up on the type, `__iter__`, `__next__`, `__getitem__` and the like, which is why
iterating a proxied list costs one request per item and per field read. The same holds for a
service client: every `ask`, `poll` or `update_records` is two requests on the pipe and one
`del` after. The measurements are in `0003-17`'s `## Decisions`.

## Options

- **The facade sends `callattr`.** The container-side stub is now the facade (`0003-21`): a
  proxy builds a path of steps, and a worker
  replays them when the path is awaited, `getattr` for an attribute step and `getattr` then
  `call` for a call step, as RPyC's proxies send them. A call step that follows an attribute
  step carries the name and the arguments together, so the worker can send one `callattr` in
  place of the pair, making a method call one request with no bound-method proxy and no `del`
  after. It is ergonomics, not enforcement (`hosted-objects.md`, "Client-side wrappers are
  ergonomics only"): the host's `callattr` handler checks the name as `getattr` does and then
  the call (`0003-16`). A call that follows no attribute step -- `await stub(...)`, or a call on
  an item step's result -- is a `call` on its target as today.
- **Leave it.** Two requests on a local pipe are a few hundred microseconds; the measurements say
  whether the halving matters for a service.

## Evaluation

The latency numbers of `0003-17`'s two-kernel and two-session runs give the per-request cost.
Worth doing if a service's per-call budget is tight enough that one round trip in two is the
difference; otherwise a note in the orientation that iterating a proxied result is a request per
item, and that a by-value result is one.
