# A host program cannot answer an escalation

## Context

`0003-24` holds a hosted call that an `escalate` rule matches until the session's escalation
handler answers, and with no handler installed `escalate` denies, the reason saying no approver is
installed. The handler is a Rust trait on the builder (`embedding.md`); `run-new` installs a
terminal prompt. A host program (`0003-29`) installs none, so a rule that escalates a call in its
session denies it. The host-program requirements list a Python handler as optional (R8): the
prototype runs under the default allow policy and needs none.

## Shape

- A handler the program installs on `outrig.Session`, carried over the owner connection: an
  `async def` receiving what the Rust handler receives -- the request's id, binding, agent,
  operation, member and host-side type, the bounded argument previews, the reason and the policy
  version -- and answering allow or deny.
- `0003-24`'s semantics unchanged: a handler that raises or returns nothing denies; an answer
  after the call was cancelled or the session closed runs nothing and is recorded as ignored;
  OutRig sets no expiry, and a handler that wants one answers deny when it passes.
- The handler runs on the program's loop, as its other coroutines do; a blocked loop holds the
  answer, which holds the call, and nothing else.

## Acceptance

- Allow runs the call once; deny raises `0003-24`'s exception in the caller; a handler that
  raises denies; an answer racing a cancel and the close records exactly one outcome.
