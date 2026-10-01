# A sender can wait for a child to have room for more requests

## Context

`requests-max` (`0003-25`; `work.md`, "Limits belong outside generated code") bounds the requests
unsettled on one child, 256 by default, and a `request()` or `submit()` past it raises
`AgentLimitReached` at once. Nothing waits outside the child's queue, and nothing in the API lets
a sender wait for room: a parent that fans out faster than its child answers either catches the
error and retries, or counts its own handles. The maintainer's name for what is missing is the
`EPOLLOUT` of an agent: a readiness signal that the child can take more.

## Shape

- An awaitable that resolves when the child's unsettled count is below `requests-max`, and at
  once when it already is. Its spelling is open: `await foo.ready_for_requests()` on an agent
  class instance, or a method on the explicit child handle, `await child.ready_for_requests()`,
  with the class form delegating to it.
- It reserves nothing. Two senders woken by one settlement race for the place, and the loser's
  `request()` raises as it does today; a sender that needs a place loops on the two.
- Cancellation-safe: cancelling the awaiting task removes its waiter and leaks nothing, and a
  waiter is settled with `AgentReleased` or the closing error when the child goes.
- It is an ordinary future to `runtime.wait`, so a sender can wait on it beside its handles and be
  ended by input as any wait is.

## Acceptance

- With `requests-max = 2` and two requests unsettled, the awaitable is pending; the child's reply
  to one resolves it, and a `request()` then succeeds.
- A task cancelled while awaiting it leaves the child's waiter set empty, and a later await works.
- Releasing the child settles every waiter on it with `AgentReleased`.
