# 0003-68 -- Events are projected by audience

## Context

Every subscriber receives the same event: `Stream::publish` builds one `Arc<Event>` under the
publish lock and hands it to every queue (`crates/outrig/src/events.rs:203-237`), the file's
included (`:440-485`). Subscriptions are made on the builder with a capacity and nothing else
(`crates/outrig/src/harness/builder.rs:289-301`; `events.rs:378-382`); `run-new`'s terminal reads
one (`crates/outrig-cli/src/cli/run_new.rs:168`) and a host program's `Session.events()` is one,
pushed as records (`crates/outrig/src/owner/session.rs:122`, `:446-462`;
`crates/outrig/src/python/outrig/_host.py:776-795`). One split exists outside the stream: a hosted
call's host traceback stays in its `hosted.ended`, kept to 16 KiB
(`crates/outrig/src/python/binding.py:1181`), and out of the one-line rendering the agent reads
(`:1405-1411`; `hosted-objects.md`, "Exceptions"). `boundary-policy.md` ("Events") says which
audience sees which fields "is `plan/next/event-audience-projections.md`", and `observability.md`
lists per-audience projections under "Not in this phase". That entry is absorbed here.

OR-OBS4 asks that prompts, source, output, bodies and tracebacks be treated as untrusted and
sensitive, that injected secrets stay out of all views, that nothing evaluate arbitrary `repr` or
properties, and notes that "Escaping is not redaction"; OR-MCP4 asks that injected secrets stay
out of "diagnostic previews". The design decided that "Args and attributes carry whatever the
library put in them ... Nothing scrubs them ... no library-neutral rule can tell a token from
text" (`hosted-objects.md`, "Exceptions"), and the conflict check (C8) and the validation
(constraint 10) found the acceptance "never appear in evidence" collides with that whenever a
hosted library echoes its environment into an exception. `diagnostics.md` ("Audiences") decides
the shape: three classifications, two subscriber audiences, classification where the event is
made, projection before delivery, the operator's file, and the honest limit; this task builds it.
No recorded decision is reversed: `hosted-objects.md`'s rule stands, since a projection withholds a
field whole and scrubs nothing.

## Goal

Every field of every event is classified where the event is made, each subscriber receives the
projection of the audience it declared, `events.jsonl` and `run-new`'s terminal take the
operator's, and the limit of field-level redaction is written where a reader will find it.

## Deliverables

- **`harness::event::Classification { Agent, Embedder, Operator }`** and a static table giving
  every field of every `Payload` variant one: *agent*, text the kernel may be sent -- a refusal's
  reason, an exception's type, OutRig's own messages; *embedder*, what a program that owns the
  session and may show records to others receives -- ids, outcomes, kinds, type names, durations,
  counts, the model's text and source, bodies on channels the embedder supplied, previews of what
  one agent gave another; *operator*, text that came through the boundary from the host side or
  names the host -- `hosted.ended.traceback`, `binding.refused.declared_in`,
  `session.report.bindings.survivors` -- and `hosted.received.arguments` and `.keywords`. A test
  holds the table complete over every variant and field, as `each_event_has_exactly_its_data_fields`
  (`crates/outrig/src/events_tests.rs:121`) holds the names, so a field added without a
  classification fails `cargo test`. Every family landed before this task is classified here; one
  landing after classifies its own, under `events.md`'s "What every event carries" (`0003-67`).
- **`harness::event::Audience { Embedder, Operator }`** and the projection: `publish` builds one
  event per audience that has a subscriber, under the same lock; in the embedder's, a field
  classified operator takes its absent form -- `None`, an empty `Vec`, an empty map -- and the
  event's new `redacted: Vec<&'static str>` names it; the operator's is the event as made, with
  `redacted` empty. `Record` writes `redacted` into `data` when it is not empty, so the owner
  protocol's pushes and any file an embedder writes carry it; the top level stays the standard
  attributes. Building a projection reads fields and calls into nothing.
- **A subscription declares its audience.** `SessionBuilder::subscribe_as(audience)` and
  `subscribe_with(options)` -- capacity and audience, and the kinds filter of fork 4 when taken;
  `subscribe()` and `subscribe_with_capacity` are the embedder's. `events.jsonl` takes the
  operator projection; `run-new`'s terminal subscription is the operator's. A host program's
  `outrig.Session(audience="embedder" | "operator")`, default `"embedder"`, crosses in
  `session.start`, and `Session.events()` yields that projection; `Session`'s docstring says so,
  and that `outrig run-new` runs a trusted program whose operator projection is its own to ask for.
- **The agent audience is a classification, not a subscription**: the table records what the
  kernel may be told, and a test holds that the error text a kernel receives for a raised hosted
  call has no traceback in it, which `binding.py` already ensures.
- **The honest limit written down**, in `events.md`'s new "Audiences" section and in `SECURITY.md`'s
  "Known boundaries": redaction is by field and never by content; a hosted library that echoes its
  environment into an exception's arguments puts it in operator fields, in the operator's file and
  in the one-line rendering the agent reads, while an embedder never receives the field; an MCP
  presenter's events are Rust-built from fields OutRig knows (`mcp-presenter.md`), so a secret
  OutRig injected is never placed in one, and content a server returns is not promised secret-free.
- **The renderer** reads the operator's file and shows `redacted` when a record carries it (a
  file an embedder wrote); the facts say which projection the file holds, read from the first
  record that carries `redacted` or from none.
- `render-session-test.py`: `every_event()` carries `redacted`; `events.md` and the `[Unreleased]`
  changelog carry the audiences and the subscription's new forms.
- No new resource can be running at the close, so no row is added to `lifecycle.md`'s close table.
- `crates/outrig/public-api.txt` regenerated: `Classification`, `Audience`, `Event::redacted`,
  `subscribe_as`, `subscribe_with` and its options.

## Acceptance

Mock-driven unless it says otherwise.

- The completeness test enumerates every `Payload` variant and every field and finds a
  classification for each; a variant added to the test's catalog without one fails it (asserted by
  a `compile_fail` or a negative case the test's construction makes).
- A hosted call that raises with a traceback holding `https://user:TOKEN@host/repo` (the fixture
  binding): the operator subscription's `hosted.ended.traceback` holds it and `redacted` is empty;
  an embedder subscription's is `None` and `redacted` is `["traceback"]`; `hosted.received` on the
  embedder's has empty `arguments` and `keywords` and names both; `events.jsonl` holds the
  traceback and the arguments; the agent's execution error names the exception's type and not the
  traceback.
- A host program's `Session.events()` by default yields the record with `data.traceback` `null` and
  `data.redacted` `["traceback"]`; with `audience="operator"`, the text and no `redacted`.
- Two embedder subscriptions and one operator's on one session: the embedder events are one
  `Arc` (`ptr_eq`), the operator's another, and each queue's `Missed` counts are its own.
- `run-new` with `[events] mode = "record"`: the file's `hosted.ended` holds the traceback; the
  terminal's subscription is the operator's (read, or a unit test on the builder it makes).
- With fork 4 taken: a subscription to `exec.submitted` alone, with capacity 1, reads every
  submission of a round that also published a hundred other events and reports no miss (the
  absorbed entry's acceptance).
- `events.md` carries "Audiences" and the limit; `SECURITY.md` carries the entry; checked by
  reading.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass; `uv run --script
  scripts/render-session-test.py` passes.

## Design forks

1. **How a withheld field is represented -- Recommended: its absent form plus `Event::redacted`.**
   Every operator-only field in the catalog is an `Option`, a `Vec` or a map, so the absent form
   exists and the typed `Payload` an embedder reads keeps its shape; `redacted` says which fields
   were withheld rather than never recorded. The alternative, a `Value` projection with the field
   removed, gives an embedder untyped data it would have to read differently from the typed
   payload it reads today.
2. **Two subscriber audiences and an agent classification -- Recommended.** The agent is not a
   subscriber; its text is built in `binding.py` and the interpreter from fields the table marks
   *agent*. The alternative, three subscriber audiences, invents a subscriber that does not exist.
3. **The default audience of `subscribe()` and `Session.events()` -- Recommended: embedder.** A
   subscription that asks for nothing gets the projection that can be shown to others; the
   operator's is asked for. The alternative, operator by default, makes every embedder that forgot
   to ask a holder of host tracebacks.
4. **Fold the kinds filter -- Recommended: yes.**
   `plan/next/a-subscription-names-the-kinds-it-reads.md` changes the same signature -- a filter
   under the publish lock so that `Missed(n)` counts only the kinds asked for -- and the 0.3.0
   release fixes it; one change to `subscribe` is cheaper than two. The alternative leaves the
   entry as it is.
5. **Argument and keyword previews operator-only -- Recommended: yes.** They are what the agent
   passed, which the model wrote, and the model may have read a token from a hosted object first;
   the absorbed entry's acceptance keeps "a full argument" for the audit record alone, and
   `boundary-policy.md` names them "the most likely to hold a secret". The alternative classifies
   them *embedder* as model-authored text, and an embedder's UI then shows them.

## Dependencies

- **Hard: `0003-66`.** The producer record is a family this task classifies.
- **Hard: `0003-67`.** "What every event carries", which this task completes with classification.
- **Hard: `0003-19`.** The stream and the subscription.
- **Hard: `0003-22`.** The hosted events and the traceback split this task puts in the table.
- **Hard: `0003-29`.** The host program's `Session` and the `session.start` request.
- **Soft: `0003-53`, `0003-56`, `0003-65`.** The external-channel, MCP and disposition families,
  classified here when they have landed, and by their own tasks when they land after.

## See also

- `plan/phase/0003-python/diagnostics.md` -- "Audiences", "The honest limit".
- `plan/phase/0003-python/boundary-policy.md` -- "Events", the warning about arguments.
- `plan/phase/0003-python/hosted-objects.md` -- "Exceptions", the rule that nothing scrubs.
- `plan/phase/0003-python/security.md` -- "The old guarantee, restated".
- `plan/phase/0003-python/observability.md` -- "Opt-in", whose file the operator's is.
- `plan/phase/0003-python/mcp-presenter.md` -- the Rust-built `mcp.*` fields.
- `crates/outrig/src/events.rs`; `crates/outrig/src/harness/event.rs`;
  `crates/outrig/src/python/binding.py`; `doc/reference/events.md`; `SECURITY.md`.
