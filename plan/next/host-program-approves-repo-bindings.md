# A host program's session refuses every repo-declared binding

## Context

A binding declared in a repository's config starts only after the operator approves that exact
declaration, and a session with no approver refuses it and publishes `binding.refused`
(`0003-20`). `run-new` installs a terminal prompt. A host program's session (`0003-29`) installs
nothing, so a program run in a repository that declares bindings gets none of them, whatever the
operator approved interactively before.

## Shape

- Read the approval store first: a declaration the operator approved in an interactive run, by
  the same digest, starts for the program without asking, which `0003-20`'s store already
  supports.
- An approver the program may install on `outrig.Session`, an `async def` receiving the
  declaration as the terminal shows it and answering yes or no, carried over the owner connection
  and run on the program's loop before the session's bindings install.
- With neither, the refusal stands, as today.

## Acceptance

- A declaration approved in an interactive run starts for a program with no approver; one not
  approved is refused with `binding.refused`; a program's approver that answers yes starts it and
  records the approval in the store.
