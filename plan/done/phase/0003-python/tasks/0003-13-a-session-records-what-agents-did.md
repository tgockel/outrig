# 0003-13 -- A session records what its agents did, by category

## Context

`doc/usage/sessions.md` opens by saying sessions exist "so you can go back and inspect what
happened", and then records the container, its connections, and the servers' stderr. The agent is
absent, and the same page still carries a TODO admitting the conversation is not persisted at all.

`observability.md` answers that, and its three settled decisions bound the work: the session
directory is the interface so nothing listens, reading is read-only, and nothing is captured
richer than its category allows. That last one replaced a single rule -- "the record holds exactly
what the model saw" -- which the event set contradicted, since a model can be told a message
exists without its body entering its context.

Most of it is already on a wire. Inter-agent messages are the exception and the one cost
co-hosting adds: they never reach the host, so the interpreter emits an observation because
someone is watching rather than because delivery requires it.

The sink is not new work either. `AuditSink` in `network.rs` is generic in everything but the
record type -- bounded queue with backpressure, reserve-then-send enqueue, an exclusive lock,
rollback to the last whole line, poisoning, bounded loss accounting. This is its second caller.

## Goal

A session writes what its agents did, in categories whose capture rules are stated, on the same
terms as the audit log that already exists.

## Deliverables

- `<session_dir>/logs/events.jsonl`, one append-only stream. One stream rather than a file per
  subject, because an agent's timeline is a causal chain and recovering the order by
  timestamp-joining files fails exactly when it is needed.
- **`AuditSink` extracted** rather than copied, since this is its second caller and
  `plan/next/dynamic-resource-mounts.md` proposes a third.
- The CloudEvents envelope, with the spec's constraint honored: attribute names are lower-case
  alphanumeric, so the flat `outrig.session_id` style `network.jsonl` uses is not a legal
  extension attribute. Everything OutRig-specific goes in `data`; `type` takes the `org.outrig.`
  prefix already used for OCI and container labels.
- **The schema for every event, and emission for the producers that exist by now**: executions
  and their results, inventories, messages, `model.round.completed` with the usage the loop
  currently discards, tool-result truncation, the effective `max-tokens` ceiling, agent lifecycle,
  interrupts and cancels, `MemoryError`, the unattributed-output bucket, and the per-call view
  manifest. Retry and failover hops get their schema here and their emission in `0003-15`, which
  is the task that adds them -- defining a schema before its producer exists is fine, and
  requiring the integration is not.
- **A configuration surface, budgeted honestly.** `observability.md` says nothing new becomes
  public, and an opt-in TOML block touches `Config`, whose fields are public and whose parser
  denies unknown keys. Event types and the writer stay private; the config key is a deliberate,
  separately stated addition rather than something the "one entry point" budget silently absorbs.
- **The three categories, each with its rule**: model view, execution diagnostics, integration
  audit.
- Opt-in, mirroring `[network].mode`, defaulting off.
- **`model.round.completed` documented as "yielded control"**, not "all work succeeded".

## Acceptance

- A session with the mode on writes a parseable stream whose exact attribute names are asserted,
  in the shape `network.rs:6113` asserts Zeek's.
- **Token usage appears**, read from `response.usage` and `completion_calls` -- which the loop
  currently drops on the floor despite already calling `.extended_details()`.
- A promotion and the per-call manifest both appear, and the manifest reconstructs what the
  provider received.
- With the mode off, no file is created.
- A record is written before teardown returns, matching the durability contract
  `sessions.md:178` states for `network.jsonl`.
- The extracted sink keeps its existing behavior: the `network.jsonl` tests pass unchanged,
  **including its bounded-shutdown contract** -- records are written or their loss is explicitly
  accounted for, and a deadline can prevent a complete flush. An overrun test, and losses reported
  as agent-event losses rather than through a network-shaped error.
- **The file's permissions are set deliberately.** This stream carries message bodies that may
  never have been printed to the model, which is a different sensitivity from a connection log.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Whether `events.jsonl` is reachable through `outrig logs` -- Recommended: no**, matching
   `network.jsonl`, which is deliberately not selectable by server name.
2. **Whether it should eventually default on -- defer.** It is the record a session was supposed
   to be, which argues yes; it is the conversation, which argues for deciding deliberately.

## Dependencies

- **Hard: 0003-05.** There is no session driving agents until the command exists.
- **Hard: 0003-12.** Manifest reconstruction is an unconditional acceptance criterion here, so its
  producer is a hard prerequisite rather than a soft one.

## See also

- `plan/phase/0003-python/observability.md` -- the categories, the envelope, and what is emitted.
- `crates/outrig/src/network.rs:1218-1799` -- `AuditSink` and `audit_writer`, the sink to extract.
- `doc/usage/sessions.md` -- the layout this extends, and the TODO it retires.

## Decisions

- **The public surface is the config block and one method (the maintainer's calls).**
  - `[events] mode = "off" | "record"`: `Config::events`, `EventsConfig` with `mode()` and
    `set_mode()`, and `EventsMode`. It merges like `[network].mode`: a repo that declares it wins,
    and a silent repo, or a bare `[events]` header, inherits.
  - `set_mode` was not in the plan. Without it, a `Config` built in code could turn recording on
    only by parsing TOML, which `[network]` has never required. It is also all a later
    `--events` flag needs.
  - `PythonAgent::shutdown(self)` finishes the log. `PythonAgent` had no shutdown step: the CLI
    dropped it. `Outrig::shutdown` could have finished the log instead, but reporting a loss
    there needed a public `OutrigError` variant. `shutdown`'s boxed error carries the
    crate-private `EventsUnwritten`, so the losses are reported as agent-event losses rather than
    through a network-shaped error.
  - Event types and the writer are crate-private. `public-api.txt` grew by exactly those nine
    lines.
  - No `run-new --events` flag. It joins `plan/next/run-new-flag-parity.md`, which already
    holds `--network`.
- **Fork 1: `events.jsonl` is not reachable through `outrig logs`**, on the grounds
  `network.jsonl` is not. Fork 2, whether to default on, stays deferred.
- **The sink is extracted, not copied: `line_sink.rs`'s `LineSink<K>`.**
  - It owns the queue, reserve-then-send, the lock, rollback, poisoning, and loss accounting,
    keyed by an owner `K`. `AuditSink` keeps only what is network's: stamping, attachment
    generations, and turning a loss into `NetworkAuditUnwritten`.
  - Every message the sink writes is rebuilt byte for byte from its `Labels`. Tracing targets
    must be constants at the call site, so each caller hands in two functions that carry its own
    target.
  - A `Loss` holds `io::Error`s, which is what every source already was. The network wrapper
    converts them to `OutrigError::Io` when it takes them.
  - What the second caller needed went into the sink rather than around it:
    - `perm` sets a mode at create, and `chmod`s the open file too, but only a regular file.
    - `open` takes the queue's capacity.
    - `try_enqueue` queues without waiting, and files a record there is no room for as a loss of
      its owner's, like any other.
    - `room()` waits until the writer holds fewer than N records, without holding a sender, which
      would keep the writer from finishing.
    - The network interceptor uses none of these but the capacity, and passes the 1,024 it had.
  - **"The network tests pass unchanged" holds with one exception.** Four sink tests built
    `AuditSink` from its fields. They now build its sink with a test constructor,
    `LineSink::queuing_to`, a construction-only edit: no assertion changed. `Deref` to the sink
    keeps every field access compiling. The test of `claims_exclusively` moved with the function
    to `line_sink_tests.rs`. Moving the rest of the sink's mechanics tests there is filed
    (`plan/next/line-sink-tests-live-in-network.md`).
  - The task's "`network.rs:6113`" is stale. The Zeek field-name test is
    `audit_record_uses_zeek_conn_field_names`, about line 5734 now.
- **A full queue: the agent waits where waiting is safe, and nothing else ever waits (the
  maintainer's call, after a first choice of "wait everywhere").**
  - The review that followed the first choice found two places where waiting changes the session
    it records:
    - A stalled interpreter reader answers the liveness probe late. That reads as a spinning
      loop, and the host interrupts healthy code.
    - A recovery path waiting before its cancel keeps Ctrl-C from stopping anything while the disk
      is stalled.
  - So `emit` is synchronous and never waits. It encodes the event, then numbers it and calls
    `try_enqueue` under one lock, so an event's id is its place in the file. An id is spent only
    on an event that was queued.
  - The events queue holds 4,096. `ready()` waits until fewer than 1,024 are held. The model loop
    calls it before each call and as the round ends, the tool before submitting, `round` before
    opening, and the user side before taking a message. A stalled disk therefore slows the agent
    rather than costing it its record.
  - An event that finds even 4,096 waiting is the sink's to count lost. `close` reports the
    sink's one loss entry: overflow, failed writes, and what an aborted writer held, together.
  - `close` is the sink's own, at two seconds. The `Events` handle holds the only sender, so live
    `Interpreter` clones never hold the writer open into the abort path. An agent dropped without
    `shutdown` still has its log finished in the background, with nobody told what was lost.
  - The first build put a buffer and a pump task in front of the sink, with its own loss count
    and close. The `/simplify` pass found that this second queue and second ledger re-implemented
    the sink's, and moved what was missing into the sink instead.
- **The envelope.**
  - `source` is `/outrig/session/<container suffix>`. That is the session `network.jsonl`
    already records, and it is not `session.json`'s id for `run-new`. The mismatch is the one
    `plan/next/run-new-container-has-no-session-label.md` already records, and a note there says
    the log inherits it.
  - `subject` is `agent/primary`, the protocol's own id; the design page's `agent/root` was a
    placeholder. Events about the shared interpreter carry no subject: `output.unattributed`,
    `interpreter.diagnostic`, and `interpreter.exited`.
  - `time` is UTC to the millisecond, from jiff.
  - Categories are documented per type, not carried in each record. Nothing OutRig-specific goes
    at the top level.
- **Turns are rig's message JSON (the maintainer's call).**
  - It is exact: ids and reasoning blocks survive. The docs say the form can change with a rig
    upgrade.
  - Read back, a rig `Text` gains `additional_params: {}`, so the reconstruction test compares
    each rebuilt call through rig's own adapters against what the mock received, not by message
    equality.
- **Where each fact is recorded is where it is known.**
  - Executions are recorded in the host. `exec.submitted` and `exec.refused` are emitted under the
    table lock before the line is queued, so nothing of an execution precedes them.
    `exec.completed` is emitted at `settle`, so a late result is recorded when it arrives. Its
    `duration` counts from an `Instant` kept in the slot.
  - The `Events` handle is a plain field of `Interpreter`, and of the table for what is recorded
    under its lock. Reaching it takes no lock, and a session that does not record builds no
    manifest, inventory, or usage list it would only drop.
  - `memory.exhausted` is detected there from a new `raised` field on the `result` line: the
    interpreter's own name for the exception's type, not the traceback's last line. A
    `MemoryError` hit while reporting a result counts too. The event carries only `execid`; the
    traceback is in `exec.completed`.
  - `exec.cancel.sent` and `exec.interrupt.sent` come from `stop()`, and only when sent.
  - Probes, abandonment, and `inventory.observed` come from `recovery`. The inventory is the
    answer `check` used to discard, so it is recorded only while an execution has run 30 seconds
    or more. The design page said "every thirty seconds" and is corrected.
  - The manifest is emitted in `History::assemble`. `on_manifest` is now test-only.
    `turn.committed` is emitted in `commit_in`, after the opening joins the round's first turn.
  - `model.round.completed` means the model yielded control. It is also used for a round OutRig
    stopped (the cap, the budget), with `stopped` set. Its usage there is summed from rig's
    per-turn `ModelTurnFinished`, since only a successful run has `completion_calls`.
    `model.round.failed` and `model.round.dropped` carry the same per-call list.
  - `round` is the conversation's number, read at the round's start. A round that commits
    nothing leaves its number to the next; documented rather than given a second counter.
  - `input_tokens_max` is named for what it is rather than called the context high-water mark.
    Anthropic's `input_tokens` excludes cache reads and OpenAI's includes them.
  - `tool.result.truncated` comes from `render_measured`. `render` and `truncate_for_llm` remain
    as test-only wrappers.
  - `model.instructions` carries the preamble, the tool definition, and the effective
    `max-tokens`. It retires `PythonAgent.max_tokens`'s dead-code expectation, and with the turns
    and manifests it rebuilds a whole request.
- **Messages carry an id, so a reader never has to correlate by order.**
  - `message.sent` gets a host-assigned id: the request id of a user's post, or a fresh one for
    an agent's send. `message.received` and `message.refused` name it.
  - A user's post is recorded before its line is written, because the agent's code can take it,
    and say so, before the post's answer arrives.
  - The agent's take is a new interpreter -> host line, `{"t": "took"}`, carrying the id the
    message was posted under, which the interpreter now keeps beside each queued delivery. The
    interpreter writes it only after the host sends `{"t": "observe"}`, which a recording host
    does at connect. So the line is emitted because someone is watching, as `observability.md`
    puts it, and existing interpreter tests see no new lines.
- **Two defects a pre-merge review found, fixed before landing.**
  - The interpreter named a result's exception through `type(e).__name__`, which runs the
    metaclass's `__getattribute__`. One that raised there escaped the handler after the slot was
    freed on the Python side and before any result went out, so the host held its slot for good:
    probes answered, and every later submission was refused, recording on or off. Both sites now
    use `_type_name`, which reads the slot on `type` itself, as the traceback formatter already
    did. `a_failure_whose_type_hides_its_name_is_still_reported` reproduces it on the old code.
  - A second review found a narrower form of the same failure. `_type_name` skips the
    metaclass, but a class's `__name__` can be set to a `str` subclass. Its `encode` override then
    ran when the result was serialized, raised, and the result was never sent. The name is now
    copied to a plain `str` with `str.__str__`, through one helper, `_raised`, used at both raise
    sites. `a_failure_whose_type_name_cannot_be_encoded_is_still_reported` reproduces it on the
    previous fix. The same review reported a launcher defect that predates this task, filed as
    `plan/next/launcher-open-is-declared-non-variadic.md`.
  - `Events::open` appended to a log that already held a recording, numbering from 1 again under
    the same container-derived `source`. A library caller that shuts one agent down and starts
    another on the same `Outrig` would have repeated `(source, id)` pairs, and turn and call ids
    too. An open now refuses a non-empty log, after taking its claim, and leaves it untouched; an
    empty one is taken. Continuing the numbering was the alternative, and was rejected: turn and
    call ids restart with the agent, so the second recording would still collide inside `data`.
- **Inter-agent messages have no producer yet.** Only the `user` channel exists, so the
  co-hosting cost the design page names stays unmeasured until subagents arrive.
- **The event queue's memory was not measured.** It holds at most 4,096 encoded events. The
  largest, a turn, is bounded by `tool-result-max` per call.
- **Verified through podman as well as on the host.** The library's e2e round and `run-new`'s
  e2e suite pass with recording on: the file is finished and `0600` when the binary exits, and its
  `source` is the container's session. Of `tests/network_interceptor.rs`, five pass. The other
  three -- every one that curls a host-side fixture from the container -- time out on this machine
  identically at the base commit, so they say nothing about the extraction.
