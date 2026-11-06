# A host program cannot approve a repo-declared binding

## Context

A binding declared in a repository's config starts only after the operator approves that exact
declaration, and a session with no approver refuses it and publishes `binding.refused`
(`0003-20`). The approval store is read before any approver is asked, so a declaration the
operator approved in an interactive `run-new` starts in a host program's session (`0003-29`) as
well. That session installs no approver, though, so a declaration not yet approved -- a new one,
or one edited since -- is refused, and the program has no way to approve it.

## Shape

- An approver the program may install on `outrig.Session`, an `async def` receiving the
  declaration as the terminal shows it and answering yes or no, carried over the owner connection
  and run on the program's loop before the session's bindings install.
- Without one, the refusal stands, as today.

## Acceptance

- A program's approver that answers yes starts the binding and records the approval in the store;
  one that answers no refuses it with `binding.refused`.
