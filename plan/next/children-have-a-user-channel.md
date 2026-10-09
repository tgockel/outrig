# The user cannot address a child agent

## Context

`plan/phase/0003-python/messages.md` designed a user channel for every agent: "The user can
address a subagent directly; nothing has to relay through its parent." `work.md` then asked how a
work item's result relates to that channel. Phase 0003 settled it the other way: children have no
user channel (`0003-26`, `plan/phase/0003-python/typed-agents.md`). A child's input is its
parent's submission, its result is a Python call, and both the REPL and the embedding API's user
channel reach the main agent only.

What that costs: a user watching a long-running child cannot redirect it, a child can tell the
user something only through its parent, and `runtime.wait` in a child has no user input to return
on.

## Shape

- Each child gets `runtime.channels["user"]`, as the main agent has, with `Delivery.sender` set
  to `user`.
- The REPL gains a form that names a child, and prints what children send with the child's name.
  Its spelling must not collide with built-in commands or `/name` skill directives (`0003-29`).
- The embedding API gives the owner each child's channel by child id, opened and closed with the
  child, with events announcing both.
- Chat never settles work: a child's result is still its `runtime.complete` call, or a reply on a
  request channel (`0003-26`, `0003-30`).

## Open questions

- Whether a message to an idle child starts a round for it, or waits for its parent's next
  submission.
- How the REPL shows several children's messages so that each stays readable.
- Whether a parent sees what its child sent to the user.
- A child made for one decorated call (`0003-27`) ends with that call, so its channel can close
  before a message sent to it arrives.

## Acceptance

- A REPL message to a running child arrives on its `runtime.channels["user"]`, and the child's
  `runtime.wait` returns on it.
- A child's send prints in the REPL with the child's name.
- No message on a child's user channel settles its submission or answers one of its requests.
