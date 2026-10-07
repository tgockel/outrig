# Embedding

How a Rust program owns an OutRig session. The program may be OutRig's own CLI or an application
that runs agents for its own tasks -- CocoClaw is the first. Either way it holds one value, the
session, and everything the session starts is stopped through it: the container, the interpreter,
each binding's host process, and the agent's rounds.

**Decided in planning (2026-09-30): the loop gets a public session API in this phase.**
`outrig run-new` is rebuilt on it and uses nothing else, and it replaces the provisional entry
point, `PythonAgent` with its `UserChannel`. This supersedes the earlier rule, in `README.md` and
`crate-split-tradeoffs.md`, that the loop has one entry point and that it is not an interface.
That rule was waiting for evidence about what a consumer needs, and two things changed: the first
milestone produced the evidence, and an embedder now needs to drive the same loop from Rust rather
than keep a second loop of its own. `0003-19` builds it.

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
starts the interpreter, resolves the model and returns the running session, and there is no
sequence of steps an embedder orders itself. Everything the session needs from outside is given to
that call, through a builder, before it starts. `run-new` makes the same call, so the CLI,
CocoClaw and any other embedder start a system the same way.

- **Configuration and the model.** A `Config`, which is what the CLI loads from TOML, and the agent
  and model to run. A model that cannot be resolved, or a secret that cannot be, fails the start
  before any container starts, as `PythonAgent::check` lets `run-new` fail today. OutRig keeps
  the operator's layer of the configuration beside the merged one and reads the operator's policy
  and the evaluator's model from it, so a repository's config cannot redefine either (`0003-22`).
  The CLI's operator layer is its global config; a `Config` an embedder passes counts as the
  operator's layer.
- **A secret resolver.** A hook the session calls to resolve each `${VAR}` its configuration
  names, for every model the session calls: the agent's, a child's, and the evaluator's. Today
  `ApiKeyRef::resolve` and `ResolvedEnvValue::resolve` read `std::env::var` directly. The CLI
  passes a resolver that reads the process environment, which keeps today's behavior. An
  embedder running many sessions in one process, each with its own key, passes a resolver per
  session and changes no environment at all.
- **Bindings** (`hosted-objects.md`), each with its name, description, factory and arguments, and
  optionally the environment and credentials its host process runs with. The CLI supplies none, so
  a binding inherits the user's environment: their ssh-agent, credential helpers and config. A
  binding given through this API needs no operator approval, because the embedder is the owner.
  Only one declared in a repository's config does, because that file is content a cloned project
  brings and the agent can write.
- **An approver for repo-declared bindings** (`0003-20`): called at session start, before anything
  is installed, with each binding declared in a repository's config that the operator has not
  approved in exactly that form. `run-new`'s approver prompts on the terminal; an embedder
  supplies its own decision, or refuses repository bindings. What a session does with no approver
  at all is `0003-20`'s fork 1.
- **An escalation handler** (`boundary-policy.md`): trusted code that receives each escalated call,
  with a cancellation, and answers allow or deny for that one call. The call waits until it is
  answered, interrupted, or the session closes. OutRig sets no expiry: a handler that wants a
  deadline answers deny when it passes.
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
| escalation      | a terminal prompt answered with `/approve <id>` or `/deny <id>`         |
| skill sources   | the project's `.agents/skills` and the user's `~/.agents/skills`        |
| subscribers     | the `events.jsonl` writer, when `[events] mode = "record"`              |
| quit            | `/quit` or EOF: `close_admission()`, `shutdown(deadline)`, a report     |
| exit status     | 0 clean; 2 stopped, with unknown outcomes; 3 not proven stopped         |

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

**The session's state is an event, not text to parse.** An embedder that shows "model working",
"Python running", "waiting for input" and "stopped" reads them from events
(`observability.md`):

- *starting* -- the container, the bindings and the interpreter are coming up;
- *idle* -- no round is running;
- *round running* -- a round is in progress, and the model is being called;
- *executing* -- within a round, a submission is running in the interpreter;
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
    .escalation(handler)
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

## Stopping

The owner stops a session with `close_admission()` and `shutdown(deadline) -> ShutdownReport`, and
neither needs the agent's cooperation. `close_admission` is not a boundary request: no policy rule
sees it, no approver is asked, it waits on nothing, and it returns at once. `shutdown` waits on
agent code at most until the deadline, then stops what is left. `lifecycle.md` has the sequence,
what it does to each resource, and what the report says.

**There is no agent-side request to finish.** Agent code cannot end its own session through any
OutRig API. An embedder whose agent should be able to say it is done builds that from its own
hosted client (below): the client's call signals the embedder's runner and returns, and the runner
calls `close_admission`, which also returns at once. Neither waits on the other, so a "finish"
request never waits on a shutdown that is waiting on it. A client call that did wait for the
shutdown would still not hold the session forever -- the drain deadline ends it -- but it would be
reported `unknown`, so a client should signal and return. What "done" means -- finished, given up,
handed to someone else -- and whether the agent may say it, are the embedder's.

## What stays the embedder's

**Who may approve.** The escalation handler decides who answers an approval. OutRig decides only
that the call waits until it is answered, interrupted, or closed. An ordinary question an agent
asks through an embedder's service and a security approval may share a UI, and they have different
authority:

|                   | an ordinary question               | a security approval                   |
|-------------------|------------------------------------|---------------------------------------|
| starts from       | the agent's call on a service      | the boundary, holding a call          |
| has the call run? | yes; asking is the operation       | no; the call waits                    |
| who may answer    | the embedder's question policy     | approvers the embedder selected       |
| free-text answer  | data for the agent                 | never read as permission              |
| no, none, or late | data, or a timeout, for the agent  | not allow; a late allow runs nothing  |
| through policy?   | yes, as a hosted call              | no; the handler calls services itself |

The last row matters most. The handler is trusted host code and calls the embedder's services
directly, so asking for an approval never needs an approval itself. A peer allowed to answer an
agent's questions is not thereby allowed to approve a host call.

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
pure-Python wheel -- platform tag `any`, ABI tag `none`, and a Python tag that includes Python 3,
as `py3-none-any` and `py2.py3-none-any` are -- constructed by its factory in a host process of its
own, and reached from agent Python as ordinary objects, with the same interception, policy and
events as any hosted object. The client talks to the service over whatever transport the embedder
already has.

Two consequences follow from that, and both are the embedder's to design around:

- **Identity comes from the environment, not from arguments.** The binding's environment, which
  the embedder sets, is where the client finds its endpoint and the identity it acts as. A task
  id the agent passes as an argument is data the client may check, not authority it acts on.
- **Waits are synchronous.** A hosted call blocks its kernel's thread, so a method that waits on a
  person holds that kernel unless the agent runs it with `asyncio.to_thread`
  (`execution-and-rounds.md`). A binding process serves each connection on its own thread, and a
  kernel keeps a pool of connections per binding (`0003-17`), so such a wait delays no other call.
  Only a binding declared `serialize = true`, because its library is not thread-safe, takes one
  call at a time, and there the wait delays every other call to that binding until it returns. At
  close such a call is a running call: it drains, and if it is still waiting at the deadline it is
  killed with its binding and reported `unknown`. The embedder knows when it closes the session, so
  its service can answer its own pending waits then and let those calls return within the drain.
  Awaitable hosted calls are `plan/next/awaitable-hosted-calls.md`.

**Why no trait now.** A trait needs a descriptor format -- methods, argument and result schemas,
and whether each call is awaitable -- and a second way of presenting an object beside hosted
Python objects, with interception, policy and events built a second time. A pure-Python client
uses the one path this phase builds. `plan/next/rust-object-as-python-object.md` records the trait,
and `mcp-wrappers.md` routes MCP servers through it.

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
service needs that the API does not give shows up there while the API can still change. No
acceptance criterion in this phase names an embedder, and CocoClaw's integration is its own to
prove; what it finds comes back as `plan/next/` entries, or as tasks `/groom-plan` numbers.

## Rejected alternatives

**Rejected: embedding by launching the CLI and reading its output.** The cheapest integration,
since `run-new` exists. Rejected because control would depend on text: whether a round ended,
whether Python is running, and whether a call waits for approval would all be parsed from lines
written for a person, and approving a call would mean typing `/approve` into a child's stdin. The
owner holds the session as a value instead, and a log is one consumer of the events, never the
control path.

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

- The module's public name and its type names -- `0003-19`'s design fork.
- Whether starting a session ensures its image. `run-new` does that itself today, before
  `Outrig::launch`, because `launch` neither pulls nor builds under the image-config's name
  (#456). If this API is all `run-new` uses, either the session takes that step or the API
  offers it.
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
  binding processes are its own, but process-wide state -- the container panic hook, the cleanup
  reaper thread -- has only run with one session in a process.
