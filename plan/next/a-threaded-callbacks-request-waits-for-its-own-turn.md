# A callback called from a worker thread waits for its own call's turn

## Context

A binding declared `serialize = true` runs one request at a time, and its `Turn` in `binding.py`
is re-entrant only for the thread whose turn it is, so a callback's nested request runs at once
when the host calls the callback on the thread serving the outer call (`0003-17`). A library that
calls back from a thread of its own -- starts a worker, has it call the container's callback, and
joins it -- breaks that. The callback's same-binding request is served away from the turn's
owner, and waits for a turn the joining thread holds until the worker ends: both wait for ever.
Only an interrupt of the nested request (cancelled at its depth since its regression was fixed),
an interrupt of the outer call, or the binding's close ends it. The documented "a callback's nested
request runs at once" does not hold for this case. It has been so since serialize landed; review
of the branch that reverted `0003-24` found it.

The test `an_interrupted_nested_request_waiting_for_the_turn_is_cancelled_alone` in
`crates/outrig/src/python/hosted_tests.rs` builds the case with the fixture's `call_in_thread`.

## Options

- **Recognize re-entry by the callback's ancestry**, not the thread. The binding already knows the
  seq of the request whose callback a thread is calling (`_context.parent`) and the parent a
  nested request names; a request whose validated parent holds the turn could share it. The
  hazard is the one `serialize` exists for: the worker's request then interleaves with the outer
  call's own thread, which a library that is not thread-safe may not survive.
- **Refuse the cross-thread nested request** with an error saying why, rather than waiting
  for ever: the callback gets an exception at once, and the library decides what follows.
- **Leave it, documented** where `serialize` is described.

## Evaluation

A worker-thread callback test that makes a same-binding request: with either fix it ends without
an interrupt -- answered under the first, refused with the stated reason under the second -- and
the outer call returns. Whether sharing the turn across threads is safe depends on the library,
which argues for refusing.
