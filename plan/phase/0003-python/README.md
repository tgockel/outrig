# 0003 -- Python

Open, and written before any of its work has started. A prototype on `prototype/python-exec`
established that the architecture runs; this page and its siblings are the design derived from
it. That branch is never merged -- its code is ported onto `version/0.3.x`, the long-running
line for this phase's work -- so nothing here should be read as describing code that exists.
The sibling documents carry the detail: `harness-components.md`, `messages.md`,
`crate-split-tradeoffs.md`, `runtime-protection.md`, `security.md`, `pyro-remote-objects.md`,
`call-inspection.md`, and `mcp-wrappers.md`.

## Goal

By the end of this phase, an agent acts by writing Python rather than by calling MCP tools. A
persistent CPython interpreter runs inside the session container, holding the agent's variables
across model turns, and the model reaches the user through a named channel instead of through
its prompt. The agent loop that drives it lives in `outrig` rather than `outrig-cli`, so that
the 0.2.x line can keep editing its own copy without conflict -- not because anything outside
the workspace is meant to call it. This is a breaking change and the line targets 0.3.0, but
the release itself belongs to a later phase: what this one is judged on is whether the thing
can be used.

## User-visible deliverables

- `outrig run-new`, an interactive session driven by a Python interpreter in the container.
  The model's only tool submits source to it. Names it binds persist across turns, so the
  conversation history is no longer the only thing carrying state. `run-legacy` is added at
  the same time as an alias for `run`, so anyone who means to stay on the existing system can
  say so before the default moves. A later milestone retargets `run` itself.
- A static CPython supplied by OutRig and mounted read-only, so the feature does not require
  a Python in the image. OutRig's other container prerequisites are unchanged and still
  apply -- `plan/next/primary-image-needs-no-sleep.md` records what a minimal image still
  cannot satisfy, and this phase does not address it. The ordinary standard
  library is available and operates on the container: `pathlib`, `open()`, `subprocess`,
  `asyncio`, `dataclasses`.
- A `user` channel the agent receives from and sends to, replacing the arrangement where a
  typed line arrives as a model prompt. Messages are announced to the model by name and count;
  reading one is an act the generated code takes.
- `runtime.wait()`, which awaits an operation while watching for input and yields to it
  without cancelling the operation, so an interruption costs a decision rather than the work
  in flight.
- An agent loop in `outrig`: model resolution, the turn loop, retry and failover, and the
  tool surface. Its modules stay private and `outrig-cli` is the only intended caller; the
  entry point it needs is the whole of the new public surface, and it is not a consumer API.
  Designing one is later work, and there is nothing yet to design it against.
- Bounded observations everywhere output can reach a model: execution output, value
  previews, the variable inventory, and tracebacks.

## Exit criteria

Not a close condition -- the phase has no end named yet, and milestones are expected to be
added as the design is worked out. What follows is the first milestone, which is what the
early tasks are written against.

- `outrig run-new` holds an interactive session end to end against a real model: the agent
  receives a typed message, runs code against the workspace, and answers, with its Python
  state surviving across turns.
- The agent loop and the interpreter host live in `outrig`, and `outrig-cli`'s existing
  harness is untouched, so a merge from the 0.2.x line applies without conflict.
- `crates/outrig/public-api.txt` grows by the one entry point `outrig-cli` calls and nothing
  else.
- `run` is unchanged and `run-legacy` reaches the same thing, so the existing system can be
  named explicitly before the default moves. Retargeting `run` is a later milestone.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` all exit 0.

## Linked subsystems

`doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`, `doc/concepts/mcp-trust-model.md`,
`doc/concepts/subagents.md`, `doc/reference/config.md`, and `doc/reference/cli.md` -- the MCP
pages most of all, since this phase demotes MCP from the way an agent acts to one integration
surface among others.

## Tasks

None are written yet. The sibling design documents settle what they are: `messages.md` decides
the channel and contract work, `harness-components.md` decides how the crate split is
sequenced, and the two deferred subjects have their own pages. Tasks will be numbered
`0003-01` onward and queued in `plan/todo/`, whose `README.md` carries the per-step index.

## Out of scope

- Surviving generated code that never yields. `runtime-protection.md` designs it and records
  what the prototype already established; only the parts the first milestone cannot run
  without are built, and the rest is a later milestone.
- Isolating credentials from the interpreter. `security.md` decides where the boundary
  actually is, `pyro-remote-objects.md` covers the mechanism across it, and
  `call-inspection.md` covers authorizing and recording what crosses -- which the transport
  does not do at all. Nothing in the first milestone depends on any of them, and the sandbox
  holds no credentials until they land.
- MCP servers presented as Python objects (`mcp-wrappers.md`). It cannot be built before the
  isolation boundary exists, for reasons that document records. Until then the servers still
  run and the model is simply not handed their tools.
- Subagents and the channels between them. `messages.md` designs the message layer for more
  than one relationship so that the shape does not have to change later, but only the `user`
  channel is built.
- Shipping the interpreter inside the binary. A developer fetches it with a script for now;
  an embedded payload is release work, not something that can be used interactively.
- The 0.3.0 release itself -- version, migration guide, public surface freeze, and the
  documentation contracts that go with them.
