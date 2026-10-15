# An interrupted hosted call made inside a callback sends no cancel line

## Context

When a hosted call is interrupted, `_Pool.request` in `interpreter.py` tells the host with a cancel
line, so a request the binding has not started -- held for a decision, or waiting for the
serialize turn -- is dropped and recorded `cancelled` (`0003-22`). It sends that line only when the
interrupted call is the outermost on its connection (`conn.depth == 1`), because `Hosted.cancel`
names every request outstanding on the connection, and a call made inside a callback shares its
connection with the call the callback belongs to, which must not be cancelled with it.

So a hosted call made inside a callback, on the callback's connection, and interrupted there sends
nothing. It cannot be waiting for the serialize turn -- the outer call's host thread holds that,
re-entrantly -- but once `0003-24` can hold a request for an approval, a nested request held that
way runs when it is approved, after its caller gave up, and is recorded as having run rather than
`cancelled`. `0003-23`'s review of the code found it; `0003-23` did not change it, since nothing
before `0003-24` holds a request.

## Options

- **Cancel the requests the interrupted call made.** `_async_request` already keeps each request's
  marks by sequence number; noting on the call's `_Wait` the sequence numbers its own requests
  took, and naming only those in the cancel line, would make the cancel right at every depth and
  drop the `depth == 1` rule.
- **Leave it.** A nested call held for a decision whose caller was interrupted is rare, and its
  record says it ran.

## Evaluation

Worth doing with `0003-24`, which is what makes a nested request wait for anything: a test there
that interrupts a callback's call while the approver holds it shows whether it ends `cancelled`.
