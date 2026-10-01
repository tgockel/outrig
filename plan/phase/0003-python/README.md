# 0003 -- Python

Open. A prototype on `prototype/python-exec` established that the architecture runs; this page
and its siblings are the design derived from it. That branch is never merged -- its code is
ported onto `version/0.3.x`, the long-running line for this phase's work. Tasks `0003-01` through
`0003-13` are merged there; everything else on these pages is design until its task is merged. The
sibling documents carry the detail: `harness-components.md`, `messages.md`,
`crate-split-tradeoffs.md`, `agent-placement.md`, `execution-and-rounds.md`,
`runtime-protection.md`, `security.md`, `history.md`, `discovery.md`, `observability.md`,
`work.md`, `hosted-objects.md`, `boundary-policy.md`, `embedding.md`, `lifecycle.md`,
`skills.md`, `typed-agents.md`, `agent-classes.md`, and `mcp-wrappers.md`. Beside them,
`potential/` holds the alternatives and improvements the phase did not adopt out of the box: each
entry names what shipped, the alternative, the evaluation that would decide between them, and when
it could land, some in 0.3.1. `/groom-plan` may turn an evaluation into a task. `plan/next/` stays
for work that is planned; `potential/` is for alternatives to evaluate.

One piece of vocabulary holds across all of them, because the old word meant two things. **A
round contains turns.** A *round* is one prompt or message, the agent working, and yielding
control back. A *turn* is one model call and the tool results it requested, which is what rig
already means by `max_turns`. `execution-and-rounds.md` owns both, including the point most easily
misread: a round may last as long as the work does, and a slow `await` inside one costs no model
calls at all.

Two more, because one word was doing both jobs. The **interpreter** is the process: one per
session, holding the protocol, the reader thread, and the memory ceiling. A **kernel** is
one agent's execution environment inside it -- its namespace, its event loop, its endpoints, its
execution slot. The sense is Jupyter's, where a kernel is a namespace you submit code to, with the
caveat that Jupyter's are separate processes and these are threads sharing one. Earlier drafts
used "kernel" for the process, which read badly beside a process-wide memory ceiling and signals.

Three more arrived with hosted objects, which `hosted-objects.md` and `boundary-policy.md` own.
A **binding** is an object the session's owner supplies to agent Python under a name -- `repo`,
say -- whose implementation runs outside the container; the name is presentation, and the binding
is the host-side record behind it. A **boundary request** is one operation agent code performs on
a hosted object: an attribute read, a call, one step of an iteration. One expression can make
several. **Admission** is the host-side gate for all new work -- a boundary request, an
execution, a child's launch -- and the owner can close it at any time.

Only the new loop and these documents adopt any of this; `outrig-cli` keeps its own.

## Goal

By the end of this phase, an agent acts by writing Python rather than by calling MCP tools. A
persistent CPython interpreter runs inside the session container, holding the agent's variables
across rounds, and the model reaches the user through a named channel instead of through
its prompt. The agent loop that drives it lives in `outrig` rather than `outrig-cli`, so that
the 0.2.x line can keep editing its own copy without conflict.

The phase then widens what that Python can reach and who can drive it. The session's owner --
the CLI, or a Rust program that embeds OutRig -- can supply host objects the agent uses as
ordinary Python, with every operation on them published as events on the session's stream and
subject to a policy the owner sets. Project skills become Python modules the main agent invokes,
and agent code can call typed child agents as functions, or keep one as an object that answers
typed requests, and combine their results with ordinary Python. The loop is driven through a
public session API that `run-new` uses like any other embedder. This is a breaking change and the
line targets 0.3.0, but the release itself belongs to a later phase: what this one is judged on is
whether the thing can be used.

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
- An agent loop in `outrig`, driven through a public session API: model resolution, the round
  loop, retry and failover, the tool surface, event subscription, and an owner-side close and
  shutdown that reports what became of every call. `run-new` uses only that API, and an embedder
  uses the same one; it may change until the 0.3.0 release fixes it. `embedding.md` and
  `lifecycle.md` design it.
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
- The same observations, recorded for a human and available to an owner. Every event is published on
  an in-memory stream an embedder can subscribe to, and with `[events] mode = "record"` the CLI
  writes it to `logs/events.jsonl`: every execution and its result, the messages that crossed
  between agents, what each round cost in tokens, every operation on a hosted object, and the
  failures that are currently reported nowhere. A script renders a session directory to one HTML
  page. `observability.md` designs it, and its governing decision is that nothing is captured richer
  than its category allows -- the model's view, execution diagnostics, and integration audit each
  carry their own rule.
- Host objects the agent uses as ordinary Python. An operator declares a binding -- a factory
  from a pure-Python package, run on the host in a process of its own -- and agent code gets the
  object under a name: a GitPython `Repo` as `repo`, say. When a session has bindings, host
  directories are mounted at their host paths, so a path means the same thing on both sides. The
  object acts with the user's authority on the host, which `security.md` states plainly.
  `hosted-objects.md` designs it.
- A policy over what crosses. Every operation on a hosted object is published as events on the
  session's stream, which the CLI writes to `events.jsonl` only when `[events] mode = "record"`.
  The default is allow, published as events; rules can deny an operation or hold it for the user,
  who answers with `/approve` or `/deny`. A model the operator enables can judge what no rule
  settled. `boundary-policy.md` designs it.
- Child agents agent code can call. `runtime.spawn` and `child.submit` create a child and give
  it work; `@outrig.agent` declares a typed function whose call runs a fresh child and returns
  a handle whose result is validated, so a workflow can fan work out and combine the results in
  Python; and `outrig.Agent` declares a class whose instance is a long-lived child answering
  typed requests on channels named after its methods, so one child keeps its context across many
  requests. `work.md`, `typed-agents.md` and `agent-classes.md` design them.
- Skills as Python modules. A skill directory may hold `skill.py` beside its `SKILL.md`, and
  `/name text` asks the main agent to run its entry function: a typed agent call maps the text to
  the entry's parameters, a dataclass derived from its signature, so no rule parses the line.
  Skills come from the project, the user, or an embedder, and are fetched only when used.
  `skills.md` designs them.

## Exit criteria

The phase closes when the second milestone is met. The first is what the early tasks were
written against.

**Milestone 1, met by `0003-05`:**

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

**Milestone 2, which closes the phase:**

- `run-new` drives its session through the public session API and nothing else; `PythonAgent` is
  gone.
- A declared binding's operations run on the host, and every one appears in the session's
  events.
- A denied operation never reaches its target, and an escalated one waits for `/approve`.
- Shutdown reports the outcome of every call it closed, `unknown` included.
- A typed child's result is validated, and an invalid one is repaired without re-running the
  work that produced it.
- One child answers several requests without losing its context: an agent class's instance
  takes a second request whose round reads what the first one bound (`0003-29`).
- `/name text` runs a skill's entry through the main agent, its parameters mapped from the text
  by a typed agent call, visible in its history.
- A skill that uses a binding, runs typed children in parallel with one repaired, and meets an
  escalated call works end to end through `run-new` (`0003-28`).
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` all exit 0.

## Linked subsystems

`doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`, `doc/concepts/mcp-trust-model.md`,
`doc/concepts/subagents.md`, `doc/reference/config.md`, `doc/reference/cli.md`, and
`SECURITY.md` -- the MCP pages most of all, since this phase demotes MCP from the way an agent
acts to one integration surface among others. Hosted objects, policy, skills and the session API
have no `doc/` page yet; `0003-30` writes them.

## Tasks

`0003-01` through `0003-30`, in `plan/done/` and `plan/todo/`; `plan/todo/README.md` carries
the per-step index. They cover the deliverables above; the subjects listed as out of scope below
have design pages or `plan/next/` entries and no tasks.

The ordering puts a usable command early. `0003-01` through `0003-05` are the shortest path to an
interactive `run-new`, and everything after is additive. One consequence is worth knowing before
reading them: `0003-05` passes a typed line to the model as an ordinary prompt, and the `user`
channel that replaces it is `0003-08`.

`0003-16` through `0003-18` are spikes. Each proves one risky part of hosted objects with real
processes before the tasks that build on it, and a spike that fails its acceptance stops and
reports to the maintainer rather than changing the design on its own. Close behavior is built with
each resource: the task that adds a resource adds its row to `lifecycle.md`'s close table.
`0003-30`, the documentation, comes last.

## Out of scope

- Surviving generated code that never yields. `runtime-protection.md` designs it and records
  what the prototype already established; only the parts the first milestone cannot run
  without are built, and the rest is a later milestone.
- Confining what a hosted object's library does on the host. A binding acts with the user's
  authority, so a library that runs programs named in agent-writable files -- host git running
  hooks and config from the workspace's `.git/` -- runs them as the user. That is documented, not
  policed (`security.md`, `plan/next/hosted-effect-confinement.md`). OutRig still starts nothing
  in the sandbox that holds a credential from its config; what the container runtime passes in on
  its own is outside that -- podman forwards the host's proxy variables, and a proxy URL can carry
  a password -- and so is what an operator's image, workspace, or mounts hold.
- Compiled hosted libraries and non-Linux hosts. A binding runs the embedded static CPython on
  the host, which loads pure-Python packages only and is built for Linux.
- MCP servers presented as Python objects (`mcp-wrappers.md`). The only MCP client that can run
  is OutRig's Rust one, and presenting a Rust object to agent Python is deferred to
  `plan/next/rust-object-as-python-object.md`. Until then `run-new` starts no MCP server.
- Restarting a dead interpreter. An interpreter that dies ends the session in this phase; any
  later continuation starts fresh with a reset notice and never replays executed code
  (`plan/next/interpreter-restart-with-a-reset-notice.md`).
- Channels between agents beyond a work item's inputs, progress and result and an agent class's
  request channels. `messages.md` designs the general layer so the shape does not have to change
  later; children have no `user` channel in this phase
  (`plan/next/children-have-a-user-channel.md`).
- A scheduler over children's active work. Two session-wide limits ship instead (`0003-25`):
  `children-max` (default 64) counts resident children, wedged ones included until they are
  reclaimed, and a launch past it raises `AgentLimitReached` at once rather than waiting; and
  `model-concurrency-max` (default 8) bounds model requests in flight, a permit held per provider
  request and never while a parent waits on a child. Neither bounds CPU or memory inside an
  admitted kernel. `subagent-depth-max` and the per-tree token budget still bound a tree, and
  `run-new` does not read `subagent-width-max`, which stays `run`'s key. A scheduler over children
  with a round in flight is `potential/resource-scheduling.md`.
- Progress channels on agent classes. A submission's handle has `h.progress`; a request on an
  agent class has no progress channel in this phase
  (`plan/next/progress-channels-on-agent-classes.md`).
- Approval reuse, a mandatory audit sink, and history export for a handed-off conversation. Each
  has a `plan/next/` entry.
- Anything live to attach to over a socket. `observability.md` settles that the session directory
  is a human's interface: a record to read, not a socket to interrogate. An embedder subscribes to
  events in memory through the session API; nothing listens.
- The 0.3.0 release itself -- version, migration guide, public surface freeze, the `run`
  retarget, and the documentation contracts that go with them.
