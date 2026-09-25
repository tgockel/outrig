# 0003 -- Python

Open, and written before any of its work has started. A prototype on `prototype/python-exec`
established that the architecture runs; this page and its siblings are the design derived from
it. That branch is never merged -- its code is ported onto `version/0.3.x`, the long-running
line for this phase's work -- so nothing here should be read as describing code that exists.
The sibling documents carry the detail: `harness-components.md`, `messages.md`,
`crate-split-tradeoffs.md`, `agent-placement.md`, `execution-and-rounds.md`,
`runtime-protection.md`, `security.md`, `history.md`, `discovery.md`, `observability.md`,
`work.md`, `pyro-remote-objects.md`, `call-inspection.md`, and `mcp-wrappers.md`.

One piece of vocabulary holds across all of them, because the old word meant two things. **A
round contains turns.** A *round* is one prompt or message, the agent working, and yielding
control back. A *turn* is one model call and the tool results it requested, which is what rig
already means by `max_turns`. `execution-and-rounds.md` owns both, including the point most easily
misread: a round may last as long as the work does, and a slow `await` inside one costs no model
calls at all.

Two more, because one word was doing both jobs. The **interpreter** is the process: one per
session, holding the protocol, the reader thread, and the address-space ceiling. A **kernel** is
one agent's execution environment inside it -- its namespace, its event loop, its endpoints, its
execution slot. The sense is Jupyter's, where a kernel is a namespace you submit code to, with the
caveat that Jupyter's are separate processes and these are threads sharing one. Earlier drafts
used "kernel" for the process, which read badly beside `RLIMIT_AS` and signals.

Only the new loop and these documents adopt any of this; `outrig-cli` keeps its own.

## Goal

By the end of this phase, an agent acts by writing Python rather than by calling MCP tools. A
persistent CPython interpreter runs inside the session container, holding the agent's variables
across rounds, and the model reaches the user through a named channel instead of through
its prompt. The agent loop that drives it lives in `outrig` rather than `outrig-cli`, so that
the 0.2.x line can keep editing its own copy without conflict -- not because anything outside
the workspace is meant to call it. This is a breaking change and the line targets 0.3.0, but
the release itself belongs to a later phase: what this one is judged on is whether the thing
can be used.

## User-visible deliverables

- `outrig run-new`, an interactive session driven by a Python interpreter in the container.
  The model's only tool submits source to it. Names it binds persist across rounds, so the
  conversation history is no longer the only thing carrying state. `run-legacy` is added at
  the same time as an alias for `run`, so anyone who means to stay on the existing system can
  say so before the default moves. A later milestone retargets `run` itself.
- A static CPython supplied by OutRig and mounted read-only, so the feature does not require a
  Python in the image. `cargo build` fetches, verifies, and embeds it; there is no setup step before
  `cargo build` and `cargo run` work. OutRig's other container prerequisites are unchanged and still
  apply -- `plan/next/primary-image-needs-no-sleep.md` records what a minimal image still cannot
  satisfy, and this phase does not address it. The ordinary standard library is available and
  operates on the container: `pathlib`, `open()`, `subprocess`, `asyncio`, `dataclasses`.
- A `user` channel the agent receives from and sends to, replacing the arrangement where a
  typed line arrives as a model prompt. Messages are announced to the model by name and count;
  reading one is an act the generated code takes.
- `runtime.wait()`, which awaits an operation while watching for input and yields to it
  without cancelling the operation, so an interruption costs a decision rather than the work
  in flight.
- An agent loop in `outrig`: model resolution, the round loop, retry and failover, and the
  tool surface. Its modules stay private and `outrig-cli` is the only intended caller; the
  entry point it needs is the whole of the new public surface, and it is not a consumer API.
  Designing one is later work, and there is nothing yet to design it against.
- Bounded observations everywhere output can reach a model: execution output, value
  previews, the variable inventory, and tracebacks.
- A stated contract for what an execution is and what its outcomes mean, including `unknown` --
  a result the host never received -- and the rule that a lost result is never retried
  automatically, because nothing here is ever rolled back. `execution-and-rounds.md` designs it,
  along with `runtime.wait` mirroring `asyncio.wait` so an agent can say "whichever of these
  finishes first" about a build and a review.
- A runtime the agent can ask about rather than guess at: signatures, docstrings, whether a call
  must be awaited, and an honest answer about what this interpreter cannot import.
  `discovery.md` designs it, and its governing constraint is that automatic observation never runs
  code the agent wrote.
- A conversation the agent can read. The full history lives in Python, where scanning it costs
  no context, and a separate view carries the subset the provider receives. The agent promotes
  what it wants kept and the rest stays reachable, so outgrowing a context window stops being the
  dead end it is today -- currently an oversized request, a 400, and every later round failing the
  same way with `/reset` the only escape. `history.md` designs it.
- The same observations, recorded for a human. A session writes `logs/events.jsonl`: every
  execution and its result, the messages that crossed between agents, what each round cost
  in tokens, and the failures that are currently reported nowhere. A script renders a session
  directory to one HTML page. `observability.md` designs it, and its governing decision is that
  nothing is captured richer than its category allows -- the model's view, execution
  diagnostics, and integration audit each carry their own rule.

## Exit criteria

Not a close condition -- the phase has no end named yet, and milestones are expected to be
added as the design is worked out. What follows is the first milestone, which is what the
early tasks are written against.

- `outrig run-new` holds an interactive session end to end against a real model: the agent
  receives a typed message, runs code against the workspace, and answers, with its Python
  state surviving across rounds.
- The agent loop and the interpreter host live in `outrig`, and `outrig-cli`'s existing
  harness is untouched, so a merge from the 0.2.x line applies without conflict.
- `crates/outrig/public-api.txt` grows by the one entry point `outrig-cli` calls and nothing
  else.
- The interpreter holds each agent's state on an agent object rather than in module globals,
  and every protocol message carries an agent id. One agent runs; nothing assumes only one can.
- `run` is unchanged and `run-legacy` reaches the same thing, so the existing system can be
  named explicitly before the default moves. Retargeting `run` is a later milestone.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` all exit 0.

## Linked subsystems

`doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`, `doc/concepts/mcp-trust-model.md`,
`doc/concepts/subagents.md`, `doc/reference/config.md`, and `doc/reference/cli.md` -- the MCP
pages most of all, since this phase demotes MCP from the way an agent acts to one integration
surface among others.

## Tasks

`0003-01` through `0003-16`, queued in `plan/todo/`, whose `README.md` carries the per-step
index. They cover the ten deliverables above; the subjects listed as out of scope below have
design pages and no tasks.

The ordering puts a usable command early. `0003-01` through `0003-05` are the shortest path to an
interactive `run-new`, and everything after is additive. One consequence is worth knowing before
reading them: `0003-05` passes a typed line to the model as an ordinary prompt, and the `user`
channel that replaces it is `0003-08`.

## Out of scope

- Surviving generated code that never yields. `runtime-protection.md` designs it and records
  what the prototype already established; only the parts the first milestone cannot run
  without are built, and the rest is a later milestone.
- Isolating credentials from the interpreter. `security.md` decides where the boundary
  actually is, `pyro-remote-objects.md` covers the mechanism across it, and
  `call-inspection.md` covers authorizing and recording what crosses -- which the transport
  does not do at all. Nothing in the first milestone depends on any of them, and until they land
  OutRig starts nothing in the sandbox that holds a credential from its config. What the
  container runtime passes in on its own is outside that -- podman forwards the host's proxy
  variables, and a proxy URL can carry a password -- and so is what an operator's image,
  workspace, or mounts hold.
- MCP servers presented as Python objects (`mcp-wrappers.md`). It cannot be built before the
  isolation boundary exists, for reasons that document records. Until then `run-new` does not
  start them: the model could not call one, and a server in the primary container would hold its
  resolved secrets beside the interpreter, as the same user.
- Subagents themselves. `work.md` designs the child and work-item API that
  `harness-components.md` records as undesigned -- two lifetimes, typed completion, and limits
  enforced outside generated code -- but none of it is built while one agent runs.
- Subagents and the channels between them. `messages.md` designs the message layer for more
  than one relationship so that the shape does not have to change later, but only the `user`
  channel is built. Where a subagent's Python *runs* is settled rather than deferred --
  `agent-placement.md` decides it, because the interpreter cannot be written to host one agent and
  later host several without being rewritten.
- Anything live to attach to. `observability.md` settles that the session directory is the
  interface: a record to read, not a socket to interrogate, and read-only either way. A streaming
  transport would not change the events, which is what makes deferring it cheap.
- The 0.3.0 release itself -- version, migration guide, public surface freeze, and the
  documentation contracts that go with them.
