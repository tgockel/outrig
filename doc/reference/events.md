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
logs. `outrig run` reads the key and records nothing. To read a recording as a page rather than as
JSON, see [Reading it in a browser](../usage/sessions.md#reading-it-in-a-browser).

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
| `subject`         | `agent/primary` for an event of the agent's; absent otherwise.        |
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
and neither do the session's own, `session.state` and `session.report`.

Inside `data`, an execution's `execid` and a message's `message` id come from one counter, which
also numbers OutRig's own questions to the interpreter, so neither runs in sequence and a gap in
them means nothing. Turns and calls are each counted from 0 on their own.

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
- **Integration audit** -- what crossed to a provider or across a channel. Provider calls are
  recorded as their token counts, never their bodies, which are model view. Channel messages are
  recorded with their bodies, at most 1 MiB each as a channel carries them. A message the user
  types can hold a secret; this category is why the file is its owner's alone.

## Model view

- `model.instructions` -- once, as the agent starts: what every call is sent besides the
  conversation.
  - `model`: the `[models.<name>]` row every call is tried against first. For an alias, that is
    the first of its models this build can reach.
  - `preamble`: the system prompt.
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
- `model.call` -- a model call, as it is made: its manifest.
  - `call`: the session's manifests counted from 0.
  - `call_id`: the model call it was assembled for, from 1, which names the call in every model
    event: a call moved to another model is assembled again under the same `call_id`.
  - `round`: its round.
  - `budget`: what it was held to: `model`; in tokens, `window`, `window_assumed`, `reserve`,
    `overhead`, and `max_tokens`, the reply ceiling the call carried, or `null`; and
    `role_alternation`, `relaxed` or `strict`, as the model's provider row says.
  - `estimate`: the whole request's estimated tokens.
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

A call that moves to another of an alias's models is assembled again for that model's window, and
recorded as a `model.call` of its own, right after the `model.failover` that moved it. Its
`budget` names the model it went to. A retry resends the same call, and records no new
`model.call`.

## Execution diagnostics

- `session.state` -- what the session is doing, as it changes: `state`, which is `starting` while
  the container and the interpreter come up, `idle` between rounds, `round_running` while a round
  calls its model, `executing` while a submission it made runs, `closing` from when the session
  is closed to new work until its shutdown report exists, and `reported` once it does. The first
  event of a session is `starting`, and the last is `reported`. Once `closing`, no state follows
  but `reported`, even while a round that was running at the close runs on. The interpreter
  exiting is `interpreter.exited` rather than a state; the session is then `closing`.
- `session.report` -- what the session's shutdown found, just before `reported`:
  - `closed_by`: what closed the session to new work: `{"by": "owner"}`, or
    `{"by": "interpreter_exited", "cause": ...}`.
  - `stopped`: `{"state": "proven"}` once the container is stopped, or
    `{"state": "not_proven", "reason": ...}` when something could not be confirmed stopped.
  - `executions`: each execution running at the close, as `{execid, status}`; `status` is `ok`
    or `error` if its result arrived, and `unknown` if it was cut off.
  - `verdict`: `clean`, `stopped_with_unknown`, or `not_proven_stopped`.
- `agent.started` -- `model`, `python` (the interpreter's version), `container`, `tool_call_max`,
  and `tool_result_max`.
- `agent.stopped` -- no fields. The agent stopped: recorded as its session's shutdown stops the
  container, before `session.report`.
- `round.started` -- `round`. A round is numbered as its first turn commits, so a round that
  fails or is interrupted before committing one leaves its number to the next.
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
  `false`, the interpreter died while the session ran, which ends the session: `closing` follows.

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
    `cancelled` for one whose round was dropped -- or `null` when it answered.
  - `usage`: what the provider reported it used, or `null`.
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
- `model.round.failed` -- a round ended by an error: `round`, `error`, and `calls`, `usage`, and
  `attempts` so far.
- `model.round.dropped` -- a round ended by a Ctrl-C while no Python ran: `round`, and `calls`,
  `usage`, and `attempts` so far.
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
- `message.sent` -- a message put on a channel: `message`, an id OutRig gives it; `channel`;
  `from` and `to`, each `user` or `agent/primary`; and `body`.
- `message.refused` -- a message the other end did not take: `message`, `channel`, `from`, `to`,
  and `reason`.
- `message.received` -- the other end took a message: `message`, the id its `message.sent` gave,
  then `channel`, `from`, and `to`. For a message to the agent, that is its code receiving it,
  which can be well after it arrived.

**Totals.** What a session spent is the sum of its `model.attempt` usage, each `attempt_id`
counted once, with a `model.usage.replaced` taking the place of its attempt's `null` and a
`model.usage.refused` changing nothing. A round's `usage` already includes the calls beneath it,
so adding a round's total to its `calls` counts each attempt twice; a round's members are its
`attempts`.

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
