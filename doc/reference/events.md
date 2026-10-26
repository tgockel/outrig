# Event Log Reference

`outrig run-new` can record what its agent did: every model call and what it was sent, every piece
of Python the model submitted and how it ended, every message on the agent's channel, and what
OutRig did along the way. The record is one file, `<session_dir>/logs/events.jsonl`, one event per
line, in the order the events happened. It is written only when the config asks for it:

```toml
[events]
mode = "record"          # off (default) | record
```

See [Config](config.md#events) for where the key can live, and
[Sessions](../usage/sessions.md#the-event-log) for how the file fits beside the session's other
logs. `outrig run` reads the key and records nothing, and with `record` set it says so at
startup. To read a recording as a page rather than as JSON, see
[Reading it in a browser](../usage/sessions.md#reading-it-in-a-browser).

The file is one reader of the session's events. A program that runs a session itself, through the
`outrig` library's `harness` module, receives the same events as values, in the same order, through
subscriptions of its own; the key decides only whether `outrig run-new` writes the file. Each
event's `data` below is the payload such a subscriber reads, field for field.

The file is created readable and writable by its owner only (mode `0600`), and an empty file
already there is narrowed to that. It holds message bodies the model may never have printed, which
is a different sensitivity from a connection log.

A file that already holds a recording is refused, and left as it is: the session does not start. Its
events would share their `source` and `id`s with the new recording's, and a reader that takes those
as an event's identity, as CloudEvents says to, would drop one recording's as repeats of the
other's. Each recording gets a log of its own, so start a fresh session to record again.

## The envelope

Each line is a [CloudEvents](https://cloudevents.io/) 1.0 event in its JSON form. Wrapped here
for reading; in the file it is one line:

```json
{"specversion": "1.0",
 "id": "41",
 "source": "/outrig/session/20260921T103000-a1b2",
 "type": "org.outrig.exec.completed",
 "subject": "agent/primary",
 "time": "2026-09-21T10:30:07.412Z",
 "datacontenttype": "application/json",
 "data": {"execid": 7, "status": "error", "duration": 1.83, "output": "building...\n",
          "dropped": 0, "error": "Traceback (most recent call last):\n...", "background": []}}
```

The top level carries the standard context attributes and nothing else. CloudEvents attribute
names are lower-case letters and digits only, so everything OutRig-specific is inside `data`.

| Attribute         | Value                                                                 |
|-------------------|-----------------------------------------------------------------------|
| `specversion`     | `"1.0"`.                                                              |
| `id`              | A decimal string counting from `"1"`: its place in the session.       |
| `source`          | `/outrig/session/<id>`, the id `session.json` and `outrig ls` show.   |
| `type`            | `org.outrig.` and the event's name, as listed below.                  |
| `subject`         | The agent it concerns: `agent/primary`, or a child's; else absent.    |
| `time`            | When the session published it: RFC 3339, UTC, to the millisecond.     |
| `datacontenttype` | `"application/json"`.                                                 |
| `data`            | The event's fields, listed below.                                     |

`source` and `id` together are unique, as CloudEvents requires. Every event the session publishes
is numbered, so a jump in `id` is events the file did not get (see
[Durability and loss](#durability-and-loss)). The session id in `source` is the
session's own: the one `session.json` and `outrig ls` show, which its containers are named for and
`network.jsonl` records as `outrig.session_id`.

The interpreter belongs to the session rather than to one agent, so events about it --
`output.unattributed`, `interpreter.diagnostic`, and `interpreter.exited` -- carry no `subject`,
and neither do the session's own, `session.state` and `session.report`, those of the process
holding its bindings, `binding.refused` and `binding.exited`, or what its skill sources hold,
`skill.discovered`. A request to a binding is the agent's that made it, and so is the import of a
skill, which code outside every execution makes for no agent.

A child agent, which an agent's code launches with `runtime.spawn`, is named `c1`, `c2`, and so on
in launch order, and its events carry its own subject, `agent/c1`: its `agent.started`,
`model.instructions`, rounds, executions, and model calls, as the main agent's carry
`agent/primary`. `agent.spawned` and each event about a request made of a child carry the child's
subject; `agent.released` carries the subject of the agent that released it.

Inside `data`, an execution's `execid` and a message's `message` id come from one counter, which
also numbers OutRig's own questions to the interpreter, so neither runs in sequence and a gap in
them means nothing. Turns and calls are each counted from 0 on their own, and requests to
bindings from 1. A request made of a child agent has a `request` id from the interpreter's own
counter, which does not run in sequence either: the id its handle and the child's delivery of it
carry in agent code. A skill invocation's `invocation` id comes from the same counter, and is its
`Invocation`'s `id` in agent code.

## Three categories

Recording is a decision with consequences: even text the model saw has a different audience and
a longer life once it is in a file. So each event belongs to one of three categories, and each
category says what it may hold.

- **Model view** -- what a model call was presented, or what a model produced, exactly as the
  model saw it. A tool result is recorded as cut to `tool-result-max`, with the same marker. This
  is the category that reconstructs a call.
- **Execution diagnostics** -- the runtime's own state, kept for debugging and never shown to the
  model: outcomes, timings, what the host did to an execution. Held to the interpreter's bounds:
  16 KiB of output and of traceback per execution, 4 KiB per line of stderr, 200 names in an
  inventory, and names and types only, never values. An execution's full output is never
  recorded.
- **Integration audit** -- what crossed to a provider, across a channel, between agents, or to a
  binding. Provider calls are recorded as their token counts, never their bodies, which are model
  view. Channel messages are recorded with their bodies, at most 1 MiB each as a channel carries
  them. What one agent gave another -- a child's inputs, a request, an answer -- is recorded as a
  preview of its JSON form, cut to 1,024 characters; a child's progress, and the message it fails
  a request with, are cut the same.
  A message the user types can hold a secret; this category is why the file is its owner's alone.
  A request to a binding is recorded with a preview of each argument, built from what crossed by
  value and cut to 200 characters, 16 items of a container, and 3 levels deep; an object of the
  host's is named by its type and a reference number, never by its `repr`, which would run its
  code.

## Model view

- `model.instructions` -- once, as an agent starts: what every call is sent besides the
  conversation.
  - `model`: the `[models.<name>]` row every call is tried against first. For an alias, that is
    the first of its models this build can reach.
  - `preamble`: the system prompt: OutRig's orientation, the agent's configured preamble, then
    the repository's instructions for agents when the agent loaded any.
  - `tools`: each tool's `name`, `description`, and `parameters` schema.
  - `max_tokens`: that model's reply ceiling, after its published ceiling filled it in or
    lowered it; `null` when none is sent. A call that moves to another of an alias's models
    carries that model's ceiling instead, which its `model.call` records.
- `turn.committed` -- a turn, as it joins the conversation. A turn is one model call and the tool
  results it asked for, so it joins once those results are in: after the executions it ran, and
  before the next call.
  - `turn`: its id, from 0, in commit order.
  - `round`: its round.
  - `incomplete`: `true` for a turn the round ended while its calls ran, whose missing results
    are OutRig's note saying so.
  - `messages`: the turn's messages, in [rig](https://docs.rs/rig-core)'s own JSON form, which can
    change when OutRig upgrades rig. The first turn of a round begins with the round's opening.
- `model.text` -- what a model said in a reply, as the call returns: its text parts joined. A reply
  with no text, only tool calls or reasoning, records none.
  - `text`: the text.
- `model.call` -- a model call, as it is made: its manifest.
  - `call`: the session's manifests counted from 0.
  - `call_id`: the model call it was assembled for, from 1, which names the call in every model
    event: a call that goes to more than one model is assembled for each under the same `call_id`.
  - `round`: its round.
  - `budget`: what it was held to: `model`; in tokens, `window`, `window_assumed`, `reserve`,
    `overhead`, and `max_tokens`, the reply ceiling the call carried, or `null`; `estimate_scale`,
    what `estimate` was multiplied by to be held to the window; and `role_alternation`,
    `relaxed` or `strict`, as the model's provider row says.
  - `estimate`: the whole request's estimated tokens, before `estimate_scale`. The scale is `1.0`
    until the model's provider has reported what a request cost. From then on it is the ratio of
    the input tokens that model's latest answered call reported -- `usage.input_tokens` on its
    `model.attempt`, with the cache's added for an Anthropic model -- to that call's `estimate`,
    and never less than `1.0` or more than `3.0`. Each model of an alias has its own. A call
    whose prompt the scale would refuse, though the estimate alone admits it, is held to a lower
    `reserve` -- and `max_tokens` with it, never below 1,024 -- and, if that is not enough, to a
    smaller `estimate_scale`, so the call is made and its answer corrects the scale.
  - `carried`: the turns it sent, oldest first, each as `{turn, why}`; `why` is `latest`,
    `round`, `promoted`, `first`, or `recent`.
  - `evicted`: the turns chosen but left out for size, the same way.
  - `withheld`: the turns chosen, and fitting, but left out so that the user's and the model's
    turns alternate, the same way. Empty unless `role_alternation` is `strict`.
  - `opening`: the round's opening message, on a round's first call, before any turn holds it;
    otherwise `null`.
  - `adjacent`: where one role follows itself in what was sent, each as `{turn, role}`; `turn` is
    `null` where the repeat is the round's opening. For a `"strict"` provider, only where the
    turn the call answers opens on the model's reply and nothing before it ends on the user's
    side, which no withholding could clear.
  - `left_out`: the parts of carried turns the call's provider cannot take, which it was not
    sent, each as `{turn, message, part}`, counted from 0 within the turn's `messages` and the
    message's `content`. Today that is reasoning without a signature, left out of a call to an
    Anthropic model, such as an OpenAI-compatible model's `reasoning_content`.
- `exec.submitted` -- Python the model submitted, as it goes to run: `execid` and `source`.

What a call sent rebuilds from the file alone: the `messages` of each turn in its `carried`, in
order, less each part its `left_out` names and any message all of whose parts it names, then its
`opening`, with `model.instructions` for the system prompt and the tool. A promotion is a request
rather than proof of what was sent; the call's `carried` is the answer. The turns themselves keep
everything each model wrote.

A call is assembled for the window of each of an alias's models it goes to, the first one's
included, and recorded as a `model.call` of its own, whose `budget` names that model. One that
moves is recorded again right after the `model.failover` that moved it. A model whose window
cannot hold the call's latest turn is passed over, with the reason in the `model.failover` that
moves the call on, and no `model.call` is recorded for it, since nothing was sent. A retry resends
the same call, and records no new `model.call`.

## Execution diagnostics

- `session.state` -- what the session is doing, as it changes: `state`, which is `starting` while
  the container and the interpreter come up, `idle` between rounds, `round_running` while a round
  calls its model, `executing` while a submission it made runs, `closing` from when the session
  is closed to new work until its shutdown report exists, and `reported` once it does. The first
  event of a session is `starting`, and the last is `reported`. Once `closing`, no state follows
  but `reported`, even while a round that was running at the close runs on. The interpreter
  exiting is `interpreter.exited` rather than a state; the session is then `closing`. A session
  with no primary agent runs no round, so it is `idle` from `starting` until `closing`, and never
  `round_running` or `executing`.
- `session.report` -- what the session's shutdown found, just before `reported`:
  - `closed_by`: what closed the session to new work: `{"by": "owner"}`, or
    `{"by": "interpreter_exited", "cause": ...}`.
  - `stopped`: `{"state": "proven"}` once the container is stopped, or
    `{"state": "not_proven", "reason": ...}` when something could not be confirmed stopped.
  - `executions`: each execution running at the close, as `{execid, status}`; `status` is `ok`
    or `error` if its result arrived, and `unknown` if it was cut off.
  - `hosted`: each request to a binding that ended after the close, in the order they ended, as
    `{request, agent, binding, outcome, reason}`, from its `hosted.ended`. One `unknown` makes
    the verdict `stopped_with_unknown`, as an execution's does.
  - `bindings`: how the process holding the session's bindings stopped, as `{names,
    group_empty, killed, survivors, exited}`, or `null` when the session had none: the bindings
    it held, whether no process is left in its process group, whether `SIGKILL` was needed after
    the grace, what was still in the group when it could not be proven empty, each as
    `<pid> <command> <state>`, and how the process ended when it ended before the stop, as its
    `binding.exited` says, or `null`. A group not proven empty makes `stopped` `not_proven`.
  - `requests`: each request made of a child agent that was open at the close, as `{request,
    child, channel, outcome}`: `channel` is `null` for a submission, and `outcome` is how it
    ended -- `closed` by the close, or `terminated` when the interpreter died first.
  - `verdict`: `clean`, `stopped_with_unknown`, or `not_proven_stopped`.
- `binding.refused` -- a binding a repository's config declared did not start, and the session
  runs without it: `binding`, its name; `declared_in`, the config file, or `null`; and
  `reason`, which says whether the operator did not approve the declaration, nobody could be
  asked, or the session had no approver, and how to approve it.
- `binding.exited` -- the process holding the session's bindings ended while the session ran:
  `bindings`, the names it held, and `cause`, such as `it was killed by SIGKILL`. The session goes
  on without them: each request the process was running ends `unknown`, and a later one raises an
  error saying how the process ended.
- `skill.discovered` -- what the session's skill sources hold, published once as it starts when it
  was given any source, read from each skill's metadata with no Python run:
  - `skills`: each skill served, as `{name, source, description, digest, executable}`; `digest`
    is of its files then, and `executable` says it has a `skill.py`.
  - `diagnostics`: each as `{skill, source, code, message}`, `skill` `null` for a source that
    could not be listed. `code` is `shadowed` for a copy an earlier source's serves in its place,
    `unknown-key` for a warning about one still served, and otherwise why the skill was left
    out: `metadata-invalid`, `duplicate-metadata-block`, or `source-error`. `message` escapes
    each control character but a line break or a tab, as `\u{1b}`.
- `skill.imported` -- a skill's package was imported in the interpreter: `skill`, `source`,
  `agent`, whose code imported it, or `null` outside every execution; `digest`, of its files then,
  or `null` when the host gave none; `reload`, true when `outrig.skills.reload` imported it again;
  and `code` and `reason`, `null` unless the import failed, such as `unsatisfied-dependency` with
  each requirement it lacks. `reason` escapes control characters as a diagnostic's `message` does.
- `skill.directive.received` -- a `/name` the user typed reached the subject on its user channel:
  `skill`, the skill it names; `source`, the source serving it; and `message`, the id its
  `message.sent` gives the message that carried it. Nothing of the skill has run: the agent's code
  invokes it, or does not.
- `skill.invocation.started` -- the subject's code called a skill's entry through
  `outrig.skills.invoke`.
  - `invocation`: its id, which the calls made inside it carry: `agent.call.started`,
    `agent.call.settled`, and `hosted.received`.
  - `skill`, `source`, and `digest`, of the skill's files when its package was imported.
  - `params`: the parameters the entry is called with, as the JSON of its `<Skill>Params`, cut to
    1,024 characters; `null` when the session's events are not observed.
  - `by`: `parser` when a child read them from what the user typed, in the typed call `call`, or
    `caller` when the code that invoked the skill gave them, `call` then `null`.
  - `bindings`: the bindings the entry's parameters were filled from.

  A parse that gives no parameters -- its answers refused, or a question for the user instead --
  starts no invocation. Its typed call's `agent.call.settled` carries the invocation's id, with
  the outcome `rejected`, or `accepted` and the question as its `value`.
- `skill.invocation.finished` -- the entry ended: `invocation`; `outcome`, `returned`, `raised`, or
  `cancelled`; `duration`, seconds from the call; `result`, the qualified name of the type of what
  it returned, and `exception`, of what it raised, each `null` otherwise; and `usage`, what the
  model requests of every child made inside the invocation used -- its parser, the typed calls
  its code made, and every agent below them -- or `null` when none reported any. A child made
  inside an invocation made inside another counts toward both. What a child spends after this is
  in the rounds' totals and not here.
- `agent.started` -- `model`, `python` (the interpreter's version), `container`, `tool_call_max`,
  and `tool_result_max`. A child's is recorded as it is launched.
- `agent.stopped` -- no fields. The agent stopped: recorded as its session's shutdown stops the
  container, before `session.report`, or for a child as its kernel ends after its release. A
  session with no primary agent records neither this nor `agent.started` for one.
- `agent.released` -- an agent's code released children: `released`, the ids it named; `ended`,
  those and every child below them, each ended or found wedged; and `requests`, the requests that
  ended `released` with them.
- `agent.wedged` -- no fields. The subject, a released child, did not stop within two seconds:
  its code holds its thread and never yields, so the thread runs until the session ends, and the
  child counts against `children-max` until then.
- `round.started` -- `round`. A round is numbered as its first turn commits, so a round that
  fails or is interrupted before committing one leaves its number to the next. Each agent counts
  its own.
- `exec.completed` -- how an execution ended, when its result arrived.
  - `execid`, and `status`: `ok`, `error`, `lost` when the interpreter exited first, or
    `refused` when the interpreter refused it.
  - `duration`: seconds from submission.
  - `output` and `dropped`: what it printed, bounded, and the bytes past the bound.
  - `error`: the traceback, or `null`.
  - `background`: output earlier executions wrote since, each as `{id, output, dropped}`.
- `exec.refused` -- a submission not run: `execid`, `holder`, and `reason`. With `reason`
  `held`, the execution `holder` held the interpreter; with `closed`, the session had been closed
  to new work, and `holder` is `null`. It has no `exec.submitted` and no `exec.completed`; its
  source is only in the turn that asked for it.
- `memory.exhausted` -- an execution raised `MemoryError`, beside its `exec.completed`: `execid`.
  The traceback is that event's `error`.
- `exec.cancel.sent` -- `execid`. A cancel sent, which Ctrl-C does: the first on code a call waits
  on, and one at the prompt on code left holding the interpreter.
- `exec.interrupt.sent` -- `execid` and `runaway`. An interrupt sent: by a Ctrl-C on code blocked
  in a call, or at the prompt on code left holding the interpreter, or by OutRig on code keeping a
  CPU busy (`runaway`).
- `exec.probe.failed` -- a liveness check the event loop did not answer: `execid`, and `verdict`,
  which is `blocked`, `spinning`, or `starved`.
- `exec.abandoned` -- OutRig stopped waiting for an execution, which keeps the interpreter until
  it ends: `execid`, and `why`, which is `user` or `runaway`.
- `inventory.observed` -- what the agent's namespace held when a liveness check was answered,
  which is only while an execution has run 30 seconds or more: `execid`; `names`, each
  `{name, type}`; `total`; and `more`, those past the 200 listed.
- `tool.result.truncated` -- a result cut to `tool-result-max`: `execid`; `size`, the bytes
  before the cut; `max`; and `kept`.
- `context.promoted`, `context.demoted` -- `turns`, the ids the agent's code named.
- `output.unattributed` -- `text`: a line no execution can be billed for, such as
  `os.write(1, ...)` or a child process started outside one.
- `interpreter.diagnostic` -- `text`: a line the interpreter wrote about itself.
- `interpreter.exited` -- `cause`: why the interpreter's output closed, and `expected`: `true`
  when the session's shutdown stopped it, recorded after `closing` and before the report. When
  `false`, the interpreter died, which ends the session: unless it was closing already, `closing`
  follows, and the report's `closed_by` names the exit -- however soon its owner closed after the
  output ended.

## Integration audit

Every model call the loop decides to make has a `call_id`, and every request actually sent for
it -- a retry, a move to another of an alias's models -- an `attempt_id` of its own. Both count
from 1 within the session. Tokens are as the provider reported them: `input_tokens`,
`output_tokens`, `total_tokens`, `cached_input_tokens`, `cache_creation_input_tokens`, and
`reasoning_tokens`. Where a provider reported nothing, the usage is `null`, never zeros.

- `model.attempt` -- one request a provider was sent, and how it ended. Every attempt is recorded
  once, failed ones included.
  - `call_id` and `attempt_id`.
  - `model`: the `[models.<name>]` row it was sent for; `identifier`: the model the request named
    on the wire; `max_tokens`: the reply ceiling it carried, or `null`.
  - `error`: why it failed -- an HTTP status, a connection error, an unusable response, or
    `cancelled` for one whose round was dropped -- or `null` when it answered. A reply with
    nothing in it that the provider ended with `stop` or `end_turn` is an answer: the model had
    nothing to add.
  - `finish_reason`: the provider's own reason the model stopped, such as `stop`, `end_turn`,
    `tool_calls`, or `length`, or `null` when its response named none or no response came back.
  - `usage`: what the provider reported it used, or `null`. A request that failed before a
    response came back reports none. An unusable response -- a `200` OutRig could not make a
    completion of -- carries the usage its body reported beside its `error`, because the provider
    bills it; it is `null` when the body reported none.
- `model.round.completed` -- the model yielded control: it finished a round, or OutRig stopped
  it. Not a claim that the round's work succeeded.
  - `round`, and `stopped`: why OutRig ended it, such as the tool-call cap, or `null`.
  - `usage`: the round's tokens: the sum over its attempts, or `null` when none reported any.
  - `calls`: each call's `{index, model, call_id, attempt_id, usage}`, `index` counted from 0.
    `model` is the `[models.<name>]` row that answered it, which for an alias is not always the
    first, and `attempt_id` the request that answered.
  - `attempts`: every request the round sent, failed ones included.
  - `input_tokens_max`: the largest `input_tokens` any call reported, or `null`. Providers differ
    on whether cached input counts there: Anthropic's does not, OpenAI's does.
  - `descendants`: the requests sent while it ran for the child agents its code launched, and
    theirs below them; `total`: its own usage and theirs, each attempt once, or `null` when none
    reported any.
- `model.round.failed` -- a round ended by an error: `round`, `error`, and `calls`, `usage`,
  `attempts`, `descendants`, and `total` so far.
- `model.round.dropped` -- a round ended by a Ctrl-C while no Python ran: `round`, and `calls`,
  `usage`, `attempts`, `descendants`, and `total` so far.
- `model.round.usage` -- requests a round's descendants sent after it ended, such as a child still
  working in a background task: `round`, `attempts`, and `usage`, their sum. Its subject is the
  agent whose round it was, and what it adds belongs to that round's `total`. One that lands as
  the round ends can come before the round's own event.
- `model.retry` -- a model call that failed and will be made again, after a wait.
  - `model`: the `[models.<name>]` row being retried.
  - `attempt`: the try that failed, counted from 1 among this call's retries.
  - `delay`: seconds until the next.
  - `error`: why, without the provider's response body: an HTTP status, a connection error, or
    an unusable response.
  - `call_id`, and `attempt_id`: the request that failed.
- `model.failover` -- a model call that moved to the next of an alias's models, once the one
  before had failed past its retries, or could not take the call: `from` and `to`, each a
  `[models.<name>]` row; `error`, why `from` was given up; `call_id`; and `attempt_id`, the last
  request `from` was sent, or `null` when it was passed over without one.
- `model.usage.replaced` -- a usage report that arrived after its attempt's event, taken: it
  replaces that attempt's `null` usage, once. `call_id`, `attempt_id`, and `usage`.
- `model.usage.refused` -- a usage report that changed nothing: `call_id` (`null` for an attempt
  the session has no record of), `attempt_id`, `usage`, and `reason`: `unknown`, `reported` when
  the attempt's own event carried its usage, or `replaced` when an earlier report already had.
- `agent.spawned` -- a child agent was launched; the subject is the child's. `name`, what the
  spawn called it; `parent`, the subject of the agent that launched it; `kind`, `work` or
  `request`; `model`, the `[models.<name>]` row as the spawn named it, or as its parent runs;
  `depth`, the main agent's being 1; `inputs`, each `{name, type, preview}`; and `channels`, a
  request child's, each `{name, request, reply}`, its types as text.
- `agent.request.sent` -- a request was made of the subject, a child: `request`; `channel`, or
  `null` for a submission; `sender`, the subject of the agent that made it; `body`, a preview of
  what was sent, a submission's inputs by name; and `attempts_max`, how many answers that fail
  their check it takes before it ends rejected.
- `agent.request.received` -- the child took it: its code received it, or a submission was given
  to it as a round began. `request`, and `round`, the child's round then, or `null`.
- `agent.request.replied` -- the child answered it: `request`; `attempt`, counted from 1; and
  `value`, a preview of the answer.
- `agent.request.invalid` -- that answer failed its check: `request`, `attempt`, and `problems`,
  each with its path.
- `agent.request.progress` -- a work child reported progress on the submission: `request` and
  `body`, its text, cut to 1,024 characters. Progress settles nothing.
- `agent.request.failed` -- `request`, and `message`, why, cut to 1,024 characters: the child's
  code failed it, or its model failed in a way that does not recover.
- `agent.request.cancelled` -- its sender cancelled it: `request`, and `received`, whether the
  child had taken it.
- `agent.request.settled` -- how it ended: `request`; `outcome`, which is `accepted`, `rejected`,
  `failed`, `cancelled`, `released`, `budget`, `closed`, or `terminated`; `attempts`, the answers
  it took; and `rounds`, the child's rounds it was open during.
- `agent.request.refused` -- an answer to a request that had already ended, refused: `request`,
  and `ended`, how it had ended.
- `agent.call.started` -- a call of a function declared with `@outrig.agent` launched the
  subject, a child, to do its work: `request`, the id of the submission the call gives it, which
  is the id the call's handle carries; `parent`, the subject of the agent whose code called;
  `module` and `qualname`, the declaration's `__module__` and `__qualname__`; and `digest`, a
  SHA-256 of the source that declared it -- its module's file, or the submission's source for a
  function declared in one -- or `null` when none could be read; and `invocation`, the skill
  invocation the call was made inside, or `null`. The parse `outrig.skills.invoke` makes is such a
  call, declared in the skill's entry module as `parse_params`. A call whose launch was refused,
  by a limit say, has no child and records neither event; its handle raises the refusal.
- `agent.call.settled` -- the call settled and its child, the subject, was released: `request`;
  `outcome`, as `agent.request.settled` says it; `usage`, the sum over every model request the
  child made, or `null` when none reported any; `invocation`, as on `agent.call.started`; and
  `value`, a preview of the result it was accepted with, cut to 1,024 characters, or `null` when it
  was not accepted or the session's events are not observed. It follows the `agent.released` that
  ended the child, except at the session's close, which ends every call `closed` at once and tears
  its children down afterward: a call settled `closed` precedes its child's release.
- `message.sent` -- a message put on a channel: `message`, an id OutRig gives it; `channel`;
  `from` and `to`, each `user` or `agent/primary`; and `body`.
- `message.refused` -- a message the other end did not take: `message`, `channel`, `from`, `to`,
  and `reason`.
- `message.received` -- the other end took a message: `message`, the id its `message.sent` gave,
  then `channel`, `from`, and `to`. For a message to the agent, that is its code receiving it,
  which can be well after it arrived.
- `hosted.received` -- a request agent code made of a binding's object, as it arrived, before
  anything is decided about it. `request`, an id OutRig gives it, which the request's other events
  carry; `agent`; `binding`; `execid`, the execution that made it, or `null`; `invocation`, the
  skill invocation it was made inside, or `null` -- each as the interpreter said, diagnostic
  context the agent's own code can change; `operation`, what it
  asks of its target; `member`, the attribute it names, or `null`; `target`, the qualified name of
  its target's type on the host, or `null`; `arguments`, a preview of each positional argument,
  and `keywords`, one per keyword argument, by name; `parent`, the request a callback made it
  under, or `null`; and `by`: `binding` when the bindings process received it, or, for one it
  never received, `relay` -- refused, or lost with the process or its connection, on its way
  there -- or `interpreter`, never sent. `operation` is `getattr`, `setattr`, `delattr`, `call`,
  `callattr`, `repr`, `str`, `cmp`, `hash`, `dir`, `instancecheck`, `buffiter`, `oldslicing` or
  `ctxexit`, each recorded every time; `ping`, `close`, `getroot`, `inspect`, `pickle` or `del`,
  bookkeeping, recorded only when refused; or `unknown`, a request type outside the protocol. A
  method call is two requests, a `getattr` and a `call`; a `for` loop is a `callattr` of
  `__iter__`, then one of `__next__` per item, the last of which raises `StopIteration`.
- `hosted.dispatched` -- `request`, `agent`, `binding`: the request's target is about to run,
  published before it does. A request whose target never runs has none.
- `hosted.ended` -- how a request ended: `request`, `agent`, `binding`; `outcome`; `reason`, for
  an outcome that has one, else `null`; `detail`, what refused it, or `null`; `exception`, the
  qualified name of what a `raised` call raised, and `traceback`, the host's traceback text, its
  last 16 KiB, which agent code does not get; `interrupted`, `true` when the caller was
  interrupted while the target ran; and `duration`, seconds from its receipt. `outcome` is:
  - `returned` or `raised` -- the target ran.
  - `refused` -- the target never ran: `interception`, the bindings process's checks refused it;
    `admission_closed`, the session was closed to new work; `queue_full`, the bindings process
    was not taking calls and the queue to it had no room, or the agent already had 1,024 requests
    in flight; or `exited`, the process had ended.
  - `cancelled` -- the target never ran: `interrupted`, its caller was interrupted first;
    `closed`, the session closed while it waited for its binding's turn or a free connection; or
    `disconnected`, its connection closed before the bindings process read it.
  - `unknown` -- no reply came: `killed`, the bindings process was killed at shutdown, or
    `exited`, it ended on its own. Whether the target ran is open.

  That a target never ran is read from `refused` or `cancelled` alone: a missing
  `hosted.dispatched` may be one a gap lost.
- `hosted.callback` -- a binding's library called a callable agent code passed it: `parent`, the
  request it was passed to; `agent`; `binding`.

**Totals.** What a session spent is the sum of its `model.attempt` usage, each `attempt_id`
counted once, with a `model.usage.replaced` taking the place of its attempt's `null` and a
`model.usage.refused` changing nothing. A round's `usage` already includes the calls beneath it,
so adding a round's total to its `calls` counts each attempt twice; a round's members are its
`attempts`. A round's `total` holds its descendants' attempts beside its own, so adding the totals
of a round and of its child's round counts the child's attempts twice; summing `usage` over every
agent's rounds counts each once.

## Durability and loss

The file is written by one of the session's subscribers, and nothing in the session waits for it.
The writer takes the next event only once the file has room for it, so when the disk falls
behind, events wait in its subscription -- up to 4,096 -- and the oldest past that are dropped and
counted. The agent never slows down for the record, and the record shows each loss as a jump in
`id`. Shutdown waits at most two seconds for the file, and everything it did not get -- dropped
while it was behind, left unread, or failed to write -- is reported as a warning, by count, with
why the first was lost. Every event the file did get is in it by the time the session's shutdown
returns; `outrig` does not `fsync` per event, so a host that loses power mid-session can lose the
tail the kernel had not written.
