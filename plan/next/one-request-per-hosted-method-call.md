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

- **A proxy subclass sends `callattr`.** The name bound for a binding is RPyC's proxy
  (`0003-23`), whose `__getattribute__` sends `getattr` for every name outside its local set and
  returns the bound method as a proxy of its own. A subclass that sent one `callattr` carrying
  the name and the arguments together, for a name whose value is then called, would make a
  method call one request with no bound-method proxy and no `del` after. It cannot know at the
  `getattr` whether a call follows, so it would have to defer that one lookup until the returned
  object is called or otherwise used -- a lazy step of exactly one attribute, which must stay
  invisible: `repo.head` read and never called still has to be the attribute's value. It is
  ergonomics, not enforcement (`hosted-objects.md`, "Client-side wrappers are ergonomics only"):
  the host's `callattr` handler checks the name as `getattr` does and then the call (`0003-16`).
- **Leave it.** Two requests on a local pipe are a few hundred microseconds; the measurements say
  whether the halving matters for a service.

## Evaluation

The latency numbers of `0003-17`'s two-kernel and two-session runs give the per-request cost.
Worth doing if a service's per-call budget is tight enough that one round trip in two is the
difference; otherwise a note in the orientation that iterating a proxied result is a request per
item, and that a by-value result is one.
