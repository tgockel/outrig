# 0003 -- Python

Open. A prototype on `prototype/python-exec` established that the architecture runs; this page
and its siblings are the design derived from it. That branch is never merged -- its code is
ported onto `version/0.3.x`, the long-running line for this phase's work. The tasks under
`plan/done/` have landed there; everything else on these pages is design until its task is
merged. The sibling documents carry the detail: `harness-components.md`, `messages.md`,
`crate-split-tradeoffs.md`, `agent-placement.md`, `execution-and-rounds.md`,
`runtime-protection.md`, `security.md`, `history.md`, `discovery.md`, `observability.md`,
`work.md`, `hosted-objects.md`, `boundary-policy.md`, `embedding.md`, `lifecycle.md`,
`skills.md`, `typed-agents.md`, `agent-classes.md` and `mcp-wrappers.md`, and, from the planning
round of 2026-10-28, `task-agents.md`, `facilities.md`, `application-code.md`, `services.md`,
`rust-owner.md`, `model-policy.md`, `external-channels.md`, `mcp-presenter.md`,
`continuation.md` and `diagnostics.md`. Beside them,
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

Three more came with the host-program requirements of 2026-10-14. The **owner** is the Rust
program holding a session: the CLI, a Rust embedder, or the engine acting for a **host program**
-- a Python script `outrig run <script.py>` supervises, which owns its session through
`outrig.Session` and has no primary agent. A **live object** is a binding whose object the host
program holds, served from its process, where a **factory binding** is built in the bindings
process from a declaration. A **root agent** is a child whose parent is the owner rather than a
kernel, in a session with no primary.

More came with the planning round of 2026-10-28, which appended the requirements of the first
application to embed the harness (`0003-39` through `0003-69`). A **facility** is an object an
agent is given to act through -- a hosted object, a service, the task object, an ordinary Python
helper -- supplied to its constructor in a dict under names; the application's word for an entry
is "capability", and the two mean the same thing. A **facility reference** is what a dict holds
for a managed facility: a value that resolves, at each use, to the calling kernel's own proxy, so
a child given its parent's entry makes its own connections and is attributed its own calls.
**Application code** is the package an owner supplies and OutRig provisions into the kernel
before construction. A **service** is a Rust object an embedder binds by name and agent Python
reaches as a generated object; the MCP presenter is a service OutRig ships. An
**owner-constructed root** is a root agent whose instance OutRig constructs in its kernel from a
class the owner names; the owner holds a **control handle** -- readiness, release, and the
**stop read**, its authoritative account of why the instance is at rest -- and never the object;
the **roster** is the owner's snapshot of the session's agents. An **external channel** is a
channel whose far end is a **backend** the owner supplies: OutRig owns the local queue, the
announcement, the wait and the delivery ids, the backend owns routing, durable storage and
identities. The **pause** is the gate that stops new work while a cut is established and reopens
if the cut is abandoned; it is not the **close**, which stays one-way. A **cut** is the point at
which saved history and saved bytes agree; a **designation** is the agent's declaration of the
files or state the cut must cover; a **package** is what a cut produces, and a **checkpoint** is
one package with a stable identity and a place in an order, handed to a **storage adapter** the
owner supplies; a **writer** is anything that can change designated state between preparation
and capture. A **save** is a checkpoint taken while the session continues, a **sleep** is a save
followed by the session's release, and a **resume** is a new session continuing from a save for
the same durable task. The **model-request limit** is a per-agent count of model requests, and
the **selection policy** is how an alias chooses its member, head-first or sticky.

Only the new loop and these documents adopt any of this; `outrig-cli` keeps its own.

## Goal

By the end of this phase, an agent acts by writing Python rather than by calling MCP tools. A
persistent CPython interpreter runs inside the session container, holding the agent's variables
across rounds, and the model reaches the user through a named channel instead of through
its prompt. The agent loop that drives it lives in `outrig` rather than `outrig-cli`, so that
the 0.2.x line can keep editing its own copy without conflict.

The phase then widens what that Python can reach and who can drive it. The session's owner --
the CLI, or a Rust program that embeds OutRig -- can supply host objects the agent uses as
ordinary Python, with every operation on them published as events on the session's stream; the
binding is the grant. Project skills become Python modules the main agent invokes,
and agent code can call typed child agents as functions, or keep one as an object that answers
typed requests, and combine their results with ordinary Python. The loop is driven through a
public session API that `run-new` uses like any other embedder. This is a breaking change and the
line targets 0.3.0, but the release itself belongs to a later phase: what this one is judged on is
whether the thing can be used.

A program can own a session too. `outrig run <script.py>` runs a trusted Python program on the
host that enters a session with no primary agent, injects objects it already holds for the agents
to use with their libraries' own calling conventions, declares agent classes and typed results in
its own module, drives them from its own asyncio loop, and reads the shutdown report; the engine
survives the program and cleans up whatever it leaves open. Fleet is the first such program, and
the mechanism is general.

An application can embed the harness from Rust and own a task-bound agent in a Session, which the
round of 2026-10-28 added from the first such application's requirements. It hands OutRig the
agent's class and the code it needs, constructs one instance for one task in a Session, and that
instance is the `self` of the Python the model writes, with the facilities the application chose
and no others advertised. The agent receives and sends on channels the application's own
messaging backs, and reaches services the application binds and MCP services the Session places,
with calls whose outcomes are honest about what may have happened. The owner reads why an agent
stopped and who is alive without depending on a lossy stream, bounds an agent's model requests,
and keeps an alias on the member that worked. The Session can be saved while it runs and put to
sleep when the application decides, with the agent choosing what state to preserve, and resumed
in a new Session that tells the model what came back and what did not. Everything the agent did
is in a stream whose records correlate and whose producer names its version. The CLI saves and
resumes through the same mechanism with local adapters, and the 0.2 container facade stays
beside the harness for an application that runs both.

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
- Host objects the agent uses as ordinary Python, with the library's own calling convention: a
  call blocks the agent's loop while the host works, and `asyncio.to_thread` keeps it turning. An
  operator declares a binding -- a factory from a pure-Python package, run on the host in the
  session's bindings process -- or a host program injects an object it already holds, and agent
  code gets the object under a name: a GitPython `Repo` as `repo`, say. When a session has
  bindings, host directories
  are mounted at their host paths, so a path means the same thing on both sides. The object acts
  with the user's authority on the host, which `security.md` states plainly. `hosted-objects.md`
  designs it.
- The binding is the grant. Every operation on a hosted object is published as events on the
  session's stream, which the CLI writes to `events.jsonl` only when `[events] mode = "record"`.
  Having the object is permission to use it, and a repository's binding needs the operator's
  approval. `boundary-policy.md` designs it.
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
- `outrig run <script.py> [args...]`: a trusted Python program on the host under the bundled
  CPython, with its own argv, cwd, streams and exit status, `import outrig`, and the
  configuration loaded; it starts no container until it asks (`0003-27`). A bare `outrig run`
  stays the legacy loop until the retarget.
- `outrig.Session`, an async context manager over a session with no primary agent (`0003-28`):
  entry starts, exit shuts down, `session.events()` observes, `session.shutdown_report` is read
  after, and `outrig.ShutdownException` is raised in the program when stop is not proven; the
  engine cleans up after a program that exits, dies or ignores a signal (`0003-29`).
- `session.inject(name, obj)`: an object the program holds, served from its process and used by
  every agent as its library documents, with `outrig.bindings[name]` as the authoritative lookup
  (`0003-36`).
- `outrig.Agent` declared and driven from the program, with typed results decoded into the
  program's own classes and repaired without rerunning work (`0003-35`), and `outrig.config()`,
  a credential-free view of the configuration (`0003-27`).
- The interpreter program ships in the mounted package, so new runtime code no longer competes
  for the argument cap (`0003-39`); the container facade keeps its items and coexists with a
  Session in one process (`0003-40`).
- An `outrig.Agent` subclass's most-derived constructor finishes before OutRig attaches it; an
  optional hook runs after; a constructor that raises spends no model round (`0003-41`).
- A constructor takes facilities beside data; each agent has its own mapping; a child's view --
  its orientation, `runtime.bindings`, its convenience globals -- is its mapping and not the
  session's (`0003-42`).
- An owner supplies application code that OutRig provisions before construction, diagnoses when it
  cannot import, and provisions again on resume; the model never installs it (`0003-43`).
- A Rust service bound by name is a Python object in every kernel, with signatures and docstrings
  from its description, every call evented and under admission (`0003-44`).
- In a kernel, the instance the parent constructed is the child's `self`; ordinary methods run
  where they are called; `@outrig.agent` methods are requests (`0003-45`).
- A Rust owner constructs the primary in its kernel from a class the application code holds, with
  a task facility and a dict, and holds a control handle (`0003-46`); the child's instructions are
  composed from OutRig's rules, the class, the application's contributions and the actual facility
  and channel manifest (`0003-47`).
- The owner awaits an authoritative stop read and the per-round facts a reply policy needs
  (`0003-48`), reads a roster of the session's agents (`0003-49`), and bounds each agent's model
  requests with warnings, a resumable stop and a reset (`0003-50`).
- An external channel with an owner-supplied backend: arrivals become pending deliveries that wake
  or announce as the definition says; sends go through the backend; a reply-owed request has one
  terminal outcome; a lost acknowledgement is uncertainty with a stable identity to resolve by
  (`0003-51` through `0003-53`).
- MCP services declared on the Session: placed, started, discovered and torn down by OutRig;
  presented to Python with exact names and structured results; calls with three dispatch states;
  URL, stdio and sidecar placements (`0003-54` through `0003-57`).
- A history format of OutRig's own, exported and imported, with opaque provider parts typed and
  carried with their provenance (`0003-58`), and an alias member that stays selected and is
  restored by name (`0003-59`).
- Assessment of the cost of sleeping, a round in which the agent designates what to preserve, a
  cut that agrees with history, truthful save and sleep outcomes, a resume that reconciles, and a
  disposition for every piece of open work (`0003-60` through `0003-65`).
- Records that name their producer, correlate where they can, and are projected by audience
  (`0003-66` through `0003-68`), and documentation of all of it (`0003-69`).

## Exit criteria

The phase closes when the third milestone is met. The first two are what the earlier tasks were
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
- Shutdown reports the outcome of every call it closed, `unknown` included.
- A typed child's result is validated, and an invalid one is repaired without re-running the
  work that produced it.
- One child answers several requests without losing its context: an agent class's instance
  takes a second request whose round reads what the first one bound (`0003-34`).
- A request sent to a child is either worked on or cancelled, and its sender is told which: no
  child is idle while it holds a request it has not answered (`0003-37`).
- `/name text` runs a skill's entry through the main agent, its parameters mapped from the text
  by a typed agent call, visible in its history.
- A skill that uses a binding and runs typed children in parallel with one repaired works end to
  end through `run-new` (`0003-33`).
- A hosted library keeps its calling convention: `repo.head.commit.hexsha` is plain code, and a
  call blocks its caller and nothing else (`0003-23`).
- `outrig run <script.py>` runs a program that enters a session with no primary agent, drives a
  host-declared agent class, has a host-declared result repaired and decoded into its own class,
  and reads the report (`0003-35`).
- A live object the program injects mid-session is used synchronously by an agent that already
  existed and by one created later (`0003-36`).
- `kill -9` of a program with a session open leaves no container running, and the engine prints
  the report (`0003-29`).
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` all exit 0.

**Milestone 3, which closes the phase** (appended 2026-10-28):

- A task-bound agent from an owner-supplied package is constructed by a Rust owner in its kernel
  with a task facility and a facility dict, and declares no request channel; the model's Python
  has it as `self`; a reviewer child built with `{files}` sees `files` in its orientation and
  nothing of its parent's task or messaging; a constructor that raises spends no model round; a
  package that cannot import fails before any inference (`0003-41` through `0003-47`).
- The owner's awaited stop read says why the agent is at rest with every event subscription
  dropped; the roster agrees with the shutdown report; an agent with a limit of three makes three
  requests and the fourth is not sent, with history and names intact until the owner resets it
  (`0003-48` through `0003-50`).
- A message routed by a backend becomes a pending delivery that wakes or announces as its
  definition says; a reply-owed request to the agent left unanswered is warned, cancelled and its
  sender told through the backend; a send whose acknowledgement is lost reports uncertainty and is
  resolvable by its identity rather than by sending again (`0003-51` through `0003-53`).
- A Rust service and URL, stdio and sidecar MCP services start through one Session facility;
  `read-file` is callable by that exact name; a multi-megabyte structured result is filtered in
  Python and a bounded slice printed; a write whose connection drops after the send is reported as
  possibly dispatched and never replayed; a rejected credential fails at once and a temporary
  outage is retried within its budget (`0003-44`, `0003-54` through `0003-57`).
- A session saves its history in OutRig's format; a sticky alias resumes on the member it was
  on, by name; the agent designates `index.json` and an operation ticket, or none, and unrelated
  dirty files are not captured; a sidecar writing a selected file is quiesced or the cut is
  refused naming it; a resume in a new session restores the declared state with a notice saying
  what was omitted; a delayed older publication cannot replace a newer one (`0003-58` through
  `0003-65`).
- A late usage record replaces its attempt once; an unknown event type renders with bounded
  detail; every record names its producer's version (`0003-66` through `0003-68`).
- The CLI saves and resumes a session through the same mechanism an embedder uses, and a facade
  container and a Session coexist in one process (`0003-40`, `0003-64`).
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` all exit 0.

## Linked subsystems

`doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`, `doc/concepts/mcp-trust-model.md`,
`doc/concepts/subagents.md`, `doc/reference/config.md`, `doc/reference/cli.md`, and
`SECURITY.md` -- the MCP pages most of all, since this phase demotes MCP from the way an agent
acts to one integration surface among others. Hosted objects, skills and the session API
and the host-program mode have no `doc/` page yet; `0003-38` writes them, and `doc/usage/run.md`
and `doc/reference/cli.md` gain the script form. The round of 2026-10-28 adds
`doc/usage/mcp.md` and `doc/concepts/mcp-servers.md`, which gain the Session's placements;
`doc/reference/config.md` and `doc/reference/events.md`, which every appended task extends; and
`SECURITY.md`, which gains what application code, a service and an MCP placement grant
(`0003-69`).

## Tasks

`0003-01` through `0003-69`, in `plan/done/` and `plan/todo/`; `plan/todo/README.md` carries
the per-step index. They cover the deliverables above; the subjects listed as out of scope below
have design pages or `plan/next/` entries and no tasks. The planning round of 2026-10-15 folded in
the host-program requirements (R1-R9): six tasks placed by dependency, and `0003-23` through
`0003-31` moved to make room. `0003-24` was reverted before release, its file kept in
`plan/done/` as the record, and `0003-25` went back to `plan/next/` as `policy-evaluator.md`. The
planning round of 2026-10-26 promoted `potential/one-shot-nudging.md` to `0003-37`, ahead of the
documentation.

The planning round of 2026-10-28 appended `0003-39` through `0003-69` from the OutRig requirements
of CocoClaw's phase 0008, the first application to embed the harness: its `requirements/outrig/`
set (OR-AG, OR-CH, OR-MCP, OR-SUS, OR-OBS) and its A-series (OR-A1 through OR-A8), validated
against these pages before they were written down. The A-series' own index calls the work a new
phase; the maintainer decided it is this one, so the tasks follow `0003-37` and the pages sit
beside these. The order follows the requirements' dependency contract: provisioning before
construction, construction before inference, channels and MCP before the cut they feed, the
history format first among the continuation tasks because every later one saves into it.
`0003-39` comes first because every later task adds interpreter code and the program is near its
argument cap; `0003-40` second because every later task adds surface beside the facade it must
leave alone. Six tasks reverse a decision these pages record, each quoting the sentence in its
`## Decisions`, and each reversed sentence carries a pointer: `0003-41` (attachment no longer in
`__init__`), `0003-42` (a per-agent view, and facilities as inputs), `0003-44` (a Rust service
trait, which `embedding.md` and `hosted-objects.md` rejected for 0.3), `0003-45` (the instance
as `self`), `0003-58` (a format of OutRig's own where the store held rig JSON), and `0003-62`
with `0003-65` (a gate that reopens, and a close that records open work before it settles it).
`0003-38` keeps its place and documents the system the first 37 tasks built; `0003-69` is the
second documentation pass, for what the appended tasks add, since a 30-task insertion is not the
kind the earlier renumberings were for. `0003-35` and `0003-36` stay as queued: the first
application is a Rust embedder and needs neither, and another program depends on both.

The ordering puts a usable command early. `0003-01` through `0003-05` are the shortest path to an
interactive `run-new`, and everything after is additive. One consequence is worth knowing before
reading them: `0003-05` passes a typed line to the model as an ordinary prompt, and the `user`
channel that replaces it is `0003-08`.

`0003-16` through `0003-18` are spikes. Each proves one risky part of hosted objects with real
processes before the tasks that build on it, and a spike that fails its acceptance stops and
reports to the maintainer rather than changing the design on its own. Close behavior is built with
each resource: the task that adds a resource adds its row to `lifecycle.md`'s close table.
`0003-38` documents what the first 37 tasks built, and `0003-69` what the round of 2026-10-28
added.

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
- ~~MCP servers presented as Python objects (`mcp-wrappers.md`).~~ Taken up on 2026-10-28: a Rust
  service is presented to agent Python by `0003-44` (`services.md`), and MCP services by
  `0003-54` through `0003-57` (`mcp-presenter.md`). `mcp-wrappers.md` records the route that was
  deferred and why the Python MCP SDK cannot run here, which still holds.
- Restarting a dead interpreter in place. An interpreter that dies ends the session. The
  continuation added on 2026-10-28 is a new session from a save, with the reset notice and no
  replay of executed code (`0003-58` through `0003-65`, `continuation.md`), never a restart in the
  same container.
- Channels between agents beyond a work item's inputs, progress and result and an agent class's
  request channels. `messages.md` designs the general layer so the shape does not have to change
  later; children have no `user` channel in this phase
  (`plan/next/children-have-a-user-channel.md`). External channels, whose far end is a backend
  the owner supplies, came in on 2026-10-28 (`0003-51` through `0003-53`,
  `external-channels.md`); an external channel a child is given is not a `user` channel.
- A scheduler over children's active work. Three limits ship instead (`0003-30`). Two are
  session-wide: `children-max` (default 64) counts resident children, wedged ones included until
  they are reclaimed, and a launch past it raises `AgentLimitReached` at once rather than waiting;
  and `model-concurrency-max` (default 8) bounds model requests in flight, a permit held per
  provider request and never while a parent waits on a child. One is per child: `requests-max`
  (default 256) counts every request of that child's not yet settled -- queued, or received and
  unanswered -- and a `request()` or `submit()` past it raises `AgentLimitReached` at once. None
  bounds CPU or memory inside an admitted kernel. `subagent-depth-max` and the per-tree token
  budget still bound a tree, and `run-new` does not read `subagent-width-max`, which stays `run`'s
  key. A scheduler over children with a round in flight is `potential/resource-scheduling.md`.
- Progress channels on agent classes. A submission's handle has `h.progress`; a request on an
  agent class has no progress channel in this phase
  (`plan/next/progress-channels-on-agent-classes.md`).
- Approval reuse and a mandatory audit sink. Each has a `plan/next/` entry. History export for a
  handed-off conversation came in on 2026-10-28 as `0003-58`'s format (`continuation.md`).
- Anything live to attach to over a socket. `observability.md` settles that the session directory
  is a human's interface: a record to read, not a socket to interrogate. An embedder subscribes to
  events in memory through the session API; nothing listens.
- The 0.3.0 release itself -- version, migration guide, public surface freeze, the `run`
  retarget, and the documentation contracts that go with them.
- An approver for a host program, and installing a program's PEP 723 dependencies. Each has a
  `plan/next/` entry; a program's session refuses repo-declared bindings meanwhile.
- Permissions finer than the binding -- "read-only git" -- and a model judging a request.
  `0003-24` built rules over a binding's methods and was reverted before release: rules in those
  terms cannot say what an operator means. `plan/next/binding-permissions.md` and
  `plan/next/policy-evaluator.md` hold the deferred work.
- Cleanup after the engine itself is killed. `lifecycle.md` records the limitation for every
  owner, and a host program changes nothing about it.
- ~~A Rust embedder injecting a live object.~~ Taken up on 2026-10-28 as a Rust service bound by
  name (`0003-44`, `services.md`); a live Python object served from a Rust embedder's process
  stays out, since a Rust embedder has no Python to serve it from.
- Killing a running hosted or live-object call (`plan/next/cancel-a-running-hosted-call.md`); an
  MCP or service call's cancel is best effort and says so (`0003-44`, `0003-56`).
- Installing third-party dependencies for application code. It is pure Python, with what it needs
  vendored or declared as a binding's packages are declared (`application-code.md`).
- Media reaching a model from Python, and rich results beyond typed data.
- Python-host parity for the operations a Rust owner gains on 2026-10-28 -- constructing the
  primary, the stop read and the roster, a channel backend -- which each task records as an open
  choice rather than a deliverable.
