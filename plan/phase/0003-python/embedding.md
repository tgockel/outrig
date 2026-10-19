# Embedding

How a Rust program owns an OutRig session. The program may be OutRig's own CLI or an application
that runs agents for its own tasks -- CocoClaw is the first. Either way it holds one value, the
session, and everything the session starts is stopped through it: the container, the interpreter,
the host process holding its bindings, and the agent's rounds. A Python program on the host owns
one the same way, through the engine: `outrig run <script.py>` runs it, and its `outrig.Session`
is a client of the engine's session over a private connection ("A Python program as owner");
nothing of the engine moves into Python.

**Decided in planning (2026-09-30): the loop gets a public session API in this phase.**
`outrig run-new` is rebuilt on it and uses nothing else, and it replaces the provisional entry
point, `PythonAgent` with its `UserChannel`. This supersedes the earlier rule, in `README.md` and
`crate-split-tradeoffs.md`, that the loop has one entry point and that it is not an interface.
That rule was waiting for evidence about what a consumer needs, and two things changed: the first
milestone produced the evidence, and an embedder now needs to drive the same loop from Rust rather
than keep a second loop of its own. `0003-19` builds it, as `outrig::harness`, with the event
catalog in `outrig::harness::event`; that task's `## Decisions` records the shape.

## Stable at 0.3.0, not before

The API is stable from the 0.3.0 release: from then on a change to it is a breaking change, and
`crates/outrig/public-api.txt`, which the existing CI job compares against every build, shows any
change in review. Until the release it may change freely on `version/0.3.x`, each change
regenerating that snapshot as changes do now. An early embedder takes it as a git dependency and
follows those changes, which is the cost of integrating before the release and the reason the
release, not this phase, fixes the shape. The release itself is a later phase (`README.md`).

No rig type appears in it. `PythonAgent` already keeps rig out of its errors by rendering them to
text, and the same rule now covers everything an embedder reads -- the round outcome, token usage,
the model that answered, and every error. OutRig names these in its own terms and converts rig's
internally, so a rig release cannot force an `outrig` major (`crate-split-tradeoffs.md`).

The history and view types in it -- a turn, a round, the manifest a model call carried, and the
selection metadata beside it -- are built to grow without a break. Each is additive: a later
addition such as an active-intent record (`potential/active-intent-record.md`, `history.md`) adds
a field or a method and changes none, and a change that cannot be additive takes a new version
beside the old rather than replacing it. `0003-19` chooses the Rust shape that keeps this true.

## What the embedder supplies

Starting a session is one call from a loaded `Config` (`0003-19`): it launches the container,
starts the interpreter, resolves the model -- for a session with a primary agent -- and returns the
running session, and there is no sequence of steps an embedder orders itself. Everything the
session needs from outside is given to that call, through a builder, before it starts. `run-new`
makes the same call, so the CLI, CocoClaw and any other embedder start a system the same way.

- **Configuration and the model.** A `Config`, which is what the CLI loads from TOML, and the agent
  and model to run. A model that cannot be resolved, or a secret that cannot be, fails the start
  before any container starts, as `PythonAgent::check` lets `run-new` fail today. A session with
  no primary agent resolves no model and asks for no secret as it starts; `check()` still resolves
  a model the builder names (`0003-28`).
- **A secret resolver.** A hook the session calls to resolve each `${VAR}` its configuration names,
  for every model the session calls: the agent's and a child's. Today `ApiKeyRef::resolve` and
  `ResolvedEnvValue::resolve` read `std::env::var` directly. The CLI passes a resolver that reads
  the process environment, which keeps today's behavior. An embedder running many sessions in one
  process, each with its own key, passes a resolver per session and changes no environment at all.
- **Bindings** (`hosted-objects.md`), each with its name, description, factory and arguments; and
  optionally the environment and credentials the one host process holding them all runs with,
  which every binding of the session shares. The CLI supplies none, so the bindings inherit the
  user's environment: their ssh-agent, credential helpers and config. A binding given through this
  API needs no operator approval, because the embedder is the owner.
  Only one declared in a repository's config does, because that file is content a cloned project
  brings and the agent can write. A host program adds live objects at run time with
  `session.inject` (`0003-36`, `hosted-objects.md`).
- **An approver for repo-declared bindings** (`0003-20`): called at session start, before anything
  is installed, with each binding declared in a repository's config that the operator has not
  approved in exactly that form. `run-new`'s approver prompts on the terminal; an embedder
  supplies its own decision, or refuses repository bindings. What a session does with no approver
  at all is what `0003-20` settled: the binding does not start, and the session runs without it,
  publishing `binding.refused`.
- **Skill sources** (`skills.md`). The project and user sources are OutRig's; an embedder can add
  its own, so a skill can come from the embedder's store rather than from a directory.
- **Event subscribers** (`observability.md`). Each receives the session's stream in sequence
  order. One that falls behind loses a counted gap and never slows the session.

What `run-new` supplies is the CLI's half of the same list, and the last two rows are how it
leaves (`0003-19`):

| input           | what `run-new` supplies                                                 |
|-----------------|-------------------------------------------------------------------------|
| configuration   | the TOML it loads, with `--agent` and `--model`                         |
| secret resolver | one that reads the process environment                                  |
| bindings        | `[bindings]` from global config, and from the repository once approved  |
| approver        | a terminal prompt at session start for each unapproved repo binding     |
| skill sources   | the project's `.agents/skills` and the user's `~/.agents/skills`        |
| subscribers     | the `events.jsonl` writer, when `[events] mode = "record"`              |
| quit            | `/quit` or EOF: `close_admission()`, `shutdown(deadline)`, a report     |
| exit status     | 0 clean; 2 stopped, with unknown outcomes; 3 not proven stopped         |

A host program supplies less, through the engine (`0003-29`): the configuration the launcher loaded;
the environment's secrets through the same resolver; a workspace and mounts as `Session`'s
arguments; live objects through `inject`; a subscription made when its session is built; and its own
control flow in place of rounds. It supplies no approver, so a repo-declared binding is refused
(`plan/next/host-program-approves-repo-bindings.md`).

**Why a resolver rather than the environment.** The environment is one table for the whole
process, shared by every session and every thread in it. An embedder that set a key, started a
session and unset the key again would race every other session reading its own, and every process
spawned meanwhile would inherit the key. Edition 2024, which this workspace uses, marks
`std::env::set_var` `unsafe` for the first of those reasons, and the workspace's own tests carry
`SAFETY` comments saying so.

## What the embedder drives

- **Rounds.** One call drives one round and returns its outcome. Today's `round()` returns the
  reply text or nothing. The outcome separates the reply from how the round ended: the model
  yielded, a limit stopped it (today written into the reply as `(round ended: <reason>)`), or
  nothing new had arrived (today's `None`). It also carries the reasoning of a round that produced
  no text, which today reads as an empty reply (`plan/next/python-agent-textless-round.md`). Its
  exact shape is `0003-19`'s.
- **User input**, through the session's end of the agent's `user` channel: what `UserChannel` does
  today -- send a message, receive what the agent sent, take what is waiting at exit.
- **An interrupter**, as `PythonAgent::interrupter` is today: it stops the Python a round waits on,
  and it is what a Ctrl-C reaches (`runtime-protection.md`).

**A session may have no primary agent** (`0003-28`). A host program's session has none: the
program constructs the agents it wants (`agent-classes.md`, "From the host program") and owns its
control flow, so there are no rounds to drive, no user channel and no interrupter. The builder asks
for one with `no_primary()`; `round()` then answers `SessionError::NoPrimary`, `user_channel()`,
`interrupter()` and `model()` answer `None`, and the session's state is `idle` from `starting`
until `closing`. Everything else -- bindings, admission, events, the ledger, children, the report
-- is the session's and works the same. No hidden primary is made to satisfy the API: the
interpreter starts with no kernel, and opens one only for an agent that exists.

**The session's state is an event, not text to parse.** An embedder that shows "model working",
"Python running", "waiting for input" and "stopped" reads them from events
(`observability.md`):

- *starting* -- the container, the bindings and the interpreter are coming up;
- *idle* -- no round is running;
- *round running* -- a round is in progress, and the model is being called; only with a primary
  agent;
- *executing* -- within a round, a submission is running in the interpreter; only with a primary
  agent;
- *closing* -- from `close_admission()` to the report: `lifecycle.md`'s closing, draining and
  terminating steps;
- *reported* -- the shutdown report exists.

The interpreter's death is an event, not a state of its own. The session then passes through
*closing* to *reported*, as for any close (`lifecycle.md`).

The same holds for smaller things. What `on_submit` does today, showing each submission as it
starts, is what the `exec.submitted` event carries, so a subscriber can do it.

A monitor built on these events sees a lossy stream: a subscriber that falls behind loses a
counted gap (`observability.md`), and one that joins late has nothing from before it joined. The
owner's own controls do not depend on the stream -- a round returns its outcome, `close_admission`
takes effect at once, and `shutdown` returns the report -- so what a late or lagging monitor lacks
is a current picture, not control. The session-rendering script is expected to become a live
monitoring page over this stream, and a coalescing latest-state snapshot, with a sequence number
to reconcile it against the events that follow, is the recovery alternative:
`potential/live-state-snapshot.md`, an entry to evaluate, not planned work.

**A round ending is not task completion.** A round ends when the model yields. Python it started
may still be running, and the embedder's task is finished only when the embedder says so, after
reading the shutdown report (`lifecycle.md`). The CLI shows the same thing: a round's closing line
names the background tasks, children and hosted requests still running, so a prompt that returns
is not presented as done (`0003-19`).

An illustrative shape, with every name and signature left to `0003-19`, except `bind(name, spec)`,
which is `0003-20`'s:

```rust
// Illustrative only.
let mut session = SessionBuilder::new(config, agent, model)
    .secrets(resolver)              // resolves each ${VAR} for this session
    .bind("tasks", task_client)     // a pure-Python client, hosted like any library
    .subscribe(sink)
    .start()
    .await?;

loop {
    tokio::select! {
        outcome = session.round() => record(outcome?),
        _ = stop.recv() => break,   // dropping the round leaves its Python running
    }
}
session.close_admission();          // returns at once
let report = session.shutdown(deadline).await;
```

## A Python program as owner

`outrig run <script.py> [args...]` (`0003-27`) runs a trusted Python program on the host under the
bundled CPython: its `__main__`, its argv, its cwd, its streams and its exit status are its own,
`import outrig` is available, and the configuration is loaded from the invocation's directory
before it runs. Starting the program starts no container and calls no model. The program decides
when to enter a session (`0003-29`):

```python
async with outrig.Session(workspace=workspace) as session:
    async with ProjectReader() as agent:
        result = await agent.explain("What does this project do?")

print(session.shutdown_report)
```

- **No start method.** Construction records inputs; async entry starts a session with no primary
  agent through the engine; async exit shuts it down, on exceptions too. A failed entry cleans up
  what it started.
- **The current session.** Entry sets a `ContextVar`; exit restores it. An agent class
  constructed without `session=` uses it, an explicit `session=` overrides it, and neither is an
  error.
- **Inputs.** `workspace` overrides the configured one; omitted, it falls back to `[workspace]`;
  with neither, nothing is mounted. `mounts` add to the configured ones. Factory bindings come from
  the configuration, repo-declared ones refused for want of an approver. Live objects come through
  `session.inject(name, obj)`, synchronous, returning once every agent can reach the object
  (`0003-36`).
- **Events.** `session.events()` is one subscription, registered when the session is built and
  read as an async iterator, with the stream's counted gaps as `Missed` records
  (`observability.md`). The report never depends on it.
- **Controls.** `await session.close_admission()`, acknowledged after the engine applied it.
- **The report.** `session.shutdown_report` after exit, a mirror of `ShutdownReport`; when the
  engine cannot prove the session stopped, exit raises `outrig.ShutdownException` in the
  program's task, with the report attached and a body exception preserved as its `__context__`.
- **Configuration.** `outrig.config()`, a read-only view of the models, aliases, providers and
  defaults the launcher loaded, with no credential in it (R7); the engine resolves names, and the
  program never parses TOML.
- **Agents.** `outrig.Agent` subclasses declared in the program and driven from it
  (`agent-classes.md`, "From the host program"; `0003-35`).

The engine survives the program (`0003-29`): a program that exits or dies with a session open has
its sessions shut down and their reports printed; one that does not respond to a forwarded SIGINT or
SIGTERM gets a configurable cooperative grace, then SIGTERM and SIGKILL to its group
(`lifecycle.md`, "The host program"). What a program does not have in this phase: rounds and a user
channel, since it has no primary; an approver, which is a `plan/next/` entry; skill sources of its
own; and the explicit child API. The Python API is fixed at the 0.3.0 release with the Rust one.

## Stopping

The owner stops a session with `close_admission()` and `shutdown(deadline) -> ShutdownReport`, and
neither needs the agent's cooperation. `close_admission` is not a boundary request: it waits on
nothing, and it returns at once. `shutdown` waits on agent code at most until the deadline, then
stops what is left. `lifecycle.md` has the sequence, what it does to each resource, and what the
report says.

**There is no agent-side request to finish.** Agent code cannot end its own session through any
OutRig API. An embedder whose agent should be able to say it is done builds that from its own
hosted client (below): the client's call signals the embedder's runner and returns, and the runner
calls `close_admission`, which also returns at once. Neither waits on the other, so a "finish"
request never waits on a shutdown that is waiting on it. A client call that did wait for the
shutdown would still not hold the session forever -- the drain deadline ends it -- but it would be
reported `unknown`, so a client should signal and return. What "done" means -- finished, given up,
handed to someone else -- and whether the agent may say it, are the embedder's.

## What stays the embedder's

**The task's transitions.** When the embedder's own task is finished, abandoned or failed, and what
it records. It makes that transition after reading the shutdown report, not when a round ends.

**One component for each decision, while an embedder migrates.** An embedder adopting this API
may keep, for a time, code of its own that does what OutRig now does -- its own loop, or its own
service beside a hosted library -- and that duplication is acceptable. One rule holds throughout:
for any running task, exactly one component admits a boundary effect, and it is OutRig, through
its admission (`lifecycle.md`); and exactly one makes the task's final transition, and it is the
embedder. With two components admitting effects, two policies could decide one task's effects, and
with two making the final transition, a task could be finished twice.

**Retries inside its own services.** OutRig never retries a hosted call. A retry hidden below a
service call can still duplicate an effect: if the embedder's client reissues a request whose
first attempt applied before its reply was lost, the effect happens twice, though OutRig sent one
call. CocoClaw's reviewer reported such retries beneath some of its own service methods. The
remedy is the embedder's: retry only operations that are idempotent or version-checked, and report
a conflict after a retry as an uncertain outcome -- the original may have succeeded -- rather than
as nothing having happened. OutRig adds no durable call ledger to cover it.

## Services written in Rust

There is no Rust trait for presenting a Rust object to agent Python in 0.3. An embedder whose
service is Rust ships a pure-Python client package and declares it as a binding, hosted like any
library (`hosted-objects.md`): installed into the binding cache with `--only-binary=:all:` from a
pure-Python wheel -- platform tag `any`, ABI tag `none`, and a Python tag that includes Python 3, as
`py3-none-any` and `py2.py3-none-any` are -- constructed by its factory in a host process of its
own, and reached from agent Python as ordinary objects, with the same interception and events as any
hosted object. The client talks to the service over whatever transport the embedder already has.

Two consequences follow from that, and both are the embedder's to design around:

- **Identity comes from the environment, not from arguments.** The bindings' environment, which
  the embedder sets, is where the client finds its endpoint and the identity it acts as. A task
  id the agent passes as an argument is data the client may check, not authority it acts on.
- **A wait blocks its caller and nothing else.** A hosted call is synchronous, so a method that
  waits on a person holds the thread that called it -- the kernel's own, or a `to_thread`
  worker's -- and one connection of the kernel's pool for the binding
  (`execution-and-rounds.md`); an agent that must stay reachable makes the call in a worker. The
  bindings process serves each connection on its own thread, and a kernel keeps a pool of
  connections per binding (`0003-17`), so such a wait delays no other call.
  Only a binding declared `serialize = true`, because its library is not thread-safe, takes one
  call at a time, and there the wait delays every other call to that binding until it returns. At
  close such a call is a running call: it drains, and if it is still waiting at the deadline it is
  killed with its binding and reported `unknown`. The embedder knows when it closes the session, so
  its service can answer its own pending waits then and let those calls return within the drain.

**Why no trait now.** A trait needs a descriptor format -- methods, argument and result schemas --
and a second way of presenting an object beside hosted Python objects, with interception and events
built a second time. A pure-Python client uses the one path this phase builds.
`plan/next/rust-object-as-python-object.md` records the trait, and `mcp-wrappers.md` routes MCP
servers through it.

## Not in this phase

- **History export and import** (`plan/next/history-export-and-import.md`): handing a conversation
  to a new session, which an embedder that gives up a task and lets another session continue it
  would want. A task run in one session does not need it.
- **A mandatory audit sink** (`plan/next/mandatory-audit-sink.md`): a subscriber whose failure to
  take an event stops admission. In this phase every subscriber is optional: one that falls behind
  loses a counted gap, and the session carries on.

## How it is proved

In two ways. `run-new` drives its session through this API and nothing else, so every test that
drives `run-new` exercises the public path, and a need the CLI has that the API does not meet
shows up as a CLI that cannot be written. That proves one foreground application and not a
service: not concurrent sessions, not a caller cancelled while its execution survives, not cleanup
after an owner dies. There is no separate service fixture for those in 0.3. CocoClaw integrates as
soon as the API exists, through a `{ git = "..." }` dependency on `version/0.3.x`, so what a
service needs that the API does not give shows up there while the API can still change. The
host-program path is the second proof: `0003-27` through `0003-29`, `0003-35` and `0003-36` each
run a real program against the engine, and R1-R9's examples are their acceptance. No acceptance
criterion in this phase names an embedder, and CocoClaw's integration is its own to prove; what
it finds comes back as `plan/next/` entries, or as tasks `/groom-plan` numbers.

## Rejected alternatives

**Rejected: embedding by launching the CLI and reading its output.** The cheapest integration, since
`run-new` exists. Rejected because control would depend on text: whether a round ended and whether
Python is running would be parsed from lines written for a person. The owner holds the session as a
value instead, and a log is one consumer of the events, never the control path.

**Rejected: keeping `PythonAgent` beside the new API.** It would leave `run-new` untouched.
Rejected because with two entry points the CLI exercises one and embedders use the other, and the
one the CLI does not exercise is the one that breaks unnoticed. `PythonAgent` was documented as
provisional so that it could be replaced.

**Rejected for 0.3: a Rust service trait**, for the reasons above. It is recorded in `plan/next/`
rather than refused.

**Rejected: secrets through the process environment.** What an embedder could do today without
any change to OutRig: set the variable, start the session, unset it. Rejected because the
environment is process-wide, so concurrent sessions read each other's keys, the write races every
thread that reads the environment, and every process started meanwhile inherits the key.

## Open questions

- ~~The module's public name and its type names.~~ Settled by `0003-19`: `outrig::harness`, a
  `SessionBuilder` that starts a `Session`, and the events in `outrig::harness::event`.
- Whether starting a session ensures its image. `run-new` does that itself today, before
  `Outrig::launch`, because `launch` neither pulls nor builds under the image-config's name
  (#456). If this API is all `run-new` uses, either the session takes that step or the API
  offers it. For a host program's session the engine ensures the image (`0003-29`); whether the
  Rust builder gains the same step stays open.
- Which of `run`'s flags `run-new` gains (`plan/next/run-new-flag-parity.md`) becomes a question
  about the builder: a flag with no builder input has no way in.
- Whether a subscriber can join a session that has already started, and what it is told about the
  events it missed. `potential/live-state-snapshot.md` is one answer to the second half.

## Unverified

- That the API can stay free of rig types across usage, model identity and the round outcome.
  Today only errors are kept free, by rendering them to text (`AgentError`).
- CocoClaw's report of retries hidden below its service methods was not checked against its code
  in this design work.
- That one process can hold several sessions at once. Each session's container, interpreter and
  bindings process are its own, but process-wide state -- the container panic hook, the cleanup
  reaper thread -- has only run with one session in a process. `0003-29`'s acceptance runs two
  at once for a host program.
