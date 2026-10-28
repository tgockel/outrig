# 0003-66 -- Records name their producer

## Context

A session's events are published to one stream, numbered gap-free at publication
(`crates/outrig/src/events.rs:203-237`), and `events.jsonl` records each in a CloudEvents 1.0
envelope whose top level carries only the standard attributes (`events.rs:572-600`;
`doc/reference/events.md`, "The envelope"). The first event of every session is `session.state`
`starting`, emitted by `Lifecycle::starting` (`crates/outrig/src/harness/lifecycle.rs:38-49`), which
every start path reaches: `SessionBuilder::start` through `open_stream`
(`crates/outrig/src/harness/builder.rs:506-527`), `start_on_host` (`:592-598`), and the test path
that hands the builder a stream. A host program receives the same records over the owner
connection (`crates/outrig/src/owner/session.rs:446-462`) as `outrig.Event` values
(`crates/outrig/src/python/outrig/_host.py:404-417`), from a subscription made as the session is
built (`:776-795`). `scripts/render-session.py` reads a session directory, shows an event of a type
it does not know as its escaped `data` with no bound (`render-session.py:466-479`), notes a
`specversion` other than `1.0` (`:856-857`), and says of an empty log that "the agent never
started" (`:829`). `scripts/render-session-test.py` sweeps a hostile payload through every field
of every kind (`every_event()`, `:327-822`) and checks the swept kinds against the catalog and
`events.md` (`:1046-1056`); `crates/outrig/src/events_tests.rs:121` holds each kind's field names.

Nothing versions the payloads. `specversion` is the envelope's, and a reader of a file cannot
tell which OutRig wrote it, nor whether a type it does not know is new or damaged. OR-OBS1 asks
to "identify producer/renderer versions and payload compatibility; CloudEvents envelope version
alone is insufficient", with the acceptance that "new event types get bounded safe detail and
unsupported-interpretation notices". OR-OBS3 adds that "Missing evidence cannot prove an agent
never started". The validation found the cluster largely met and named "producer-version fields"
as the first addition. `plan/phase/0003-python/diagnostics.md` ("Records name their
producer") decides the record, the schema's version rule and the reader's rule; this task builds
them. No recorded decision is reversed: the sentences that change are reference statements, listed
below.

## Goal

Every session's stream and file open with a record naming OutRig's name and version and the event
schema's version, and a reader meeting a type or a version it does not know keeps the record,
shows it with bounded detail and a notice, and never drops it.

## Deliverables

- **`session.producer`**, a new `Payload` variant in `harness::event`, published first: `name`,
  `"outrig"`; `version`, `env!("CARGO_PKG_VERSION")`; `schema`, `harness::event::SCHEMA`. No
  subject. `#[non_exhaustive]`, like every variant: `0003-67` adds `task`. Emitted in
  `Lifecycle::starting` ahead of `starting`, so it is id 1 of every stream, line 1 of every file,
  and the first `Event` a host program's `Session.events()` yields, on every start path.
- **`harness::event::SCHEMA: u32`**, `1`, with its rule in its doc and in `events.md`: an additive
  change -- a kind, a field, a variant -- keeps the number; a field removed or renamed, or one
  whose meaning changes, moves it. A test asserts the constant equals what the producer record
  carries.
- **`Session::producer() -> &Producer`**, a `#[non_exhaustive]` struct with the three fields, and
  a host program's `session.producer`, an `outrig.Producer` frozen dataclass set at entry from the
  `session.start` answer, which gains `producer`; `Producer` joins `_HOST_ONLY` (`__init__.py:64`).
- **The reader's rule in `events.md`**, in a new "Versions and the producer record" section: a
  reader keeps a record of a type it does not know, shows its `data` as data, bounded, with a
  notice; a `schema` greater than it knows gets a notice; a file with no producer record was
  written by an OutRig before this task and is read, with a note. `events.md`'s `session.state`
  entry stops saying `starting` is the first event, and the envelope section says the producer
  record is.
- **The renderer applies it**: the facts table shows the producer's name, version and schema; a
  type with no handler renders the kind as recorded, its `data` cut to a bound with the omitted
  size said, and the notice "this renderer does not interpret this event type"; a `schema` above
  the renderer's own constant adds a health note; a log with no producer record adds a health note
  and renders; the empty-log note becomes a statement that nothing was recorded and that this
  does not show whether an agent started; each timeline row names its record's id, so a reader can
  find the line in `events.jsonl`. `--host` serves the same page.
- **`render-session-test.py`**: `every_event()` gains `session.producer`; cases for the bounded
  unknown type, the newer schema, the missing producer record, the empty-log wording and the row
  ids.
- **Reference statements changed**: `events.md`, `session.state` ("The first event of a session is
  `starting`"); `_host.py:786` ("the first a `session.state` of `starting`"); `lifecycle.rs:37` ("A
  session starting: the first event it publishes says so"); `doc/usage/sessions.md`, "Reading it in
  a browser", gains one sentence on what the page says of a type it does not know.
- `crates/outrig/CHANGELOG.md`, `[Unreleased]`: the record, the constant, `Session::producer`.
- No new resource can be running at the close, so no row is added to `lifecycle.md`'s close table.
- `crates/outrig/public-api.txt` regenerated: the variant, `SCHEMA`, `Producer`,
  `Session::producer`.

## Acceptance

Mock-driven unless it says otherwise.

- A session started through `SessionBuilder` with a subscription made on the builder receives
  `session.producer` as id 1 with no subject and `session.state` `starting` as id 2; with
  `record_events`, line 1 of `events.jsonl` has `type` `org.outrig.session.producer`, no
  `subject`, `data.name` `"outrig"`, `data.version` equal to `env!("CARGO_PKG_VERSION")` and
  `data.schema` equal to `SCHEMA`. The same holds for a session with no primary agent and for the
  test path that hands the builder a stream.
- `Session::producer()` equals the record's fields. A host program's `Session.events()` yields
  the record as its first `Event`, and `session.producer` equals it after entry; a program that
  never calls `events()` still reads `session.producer`.
- The renderer, over a log opening with the record, shows the three facts; over `ordinary()` with
  a `session.producer` whose `schema` is `99`, adds the newer-schema health note and renders every
  round; over a log with no producer record, adds the missing-record note and renders; over
  `ordinary()` plus an event of an unknown type whose `data` is 1 MiB of text, renders a block no
  longer than the bound, says how many bytes were omitted, carries the notice, and shows the kind
  as recorded, escaped; over an empty log, says nothing was recorded and does not say the agent
  never started. Each timeline row of `ordinary()` carries its record's id.
- `events_tests.rs`'s field-name test covers the variant;
  `test_the_sweep_covers_every_event_there_is` passes with the new kind swept;
  `test_every_string_field_is_escaped_and_shown` finds the three fields on the page.
- `events.md` carries the "Versions" section and the reader's rule, and no longer says `starting`
  is the first event; `_host.py`'s and `lifecycle.rs`'s sentences are changed; checked by reading.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass; `uv run --script
  scripts/render-session-test.py` passes.

## Design forks

1. **The record's name -- Recommended: `session.producer`.** It is about the session's stream, has
   no subject, and sits beside `session.state` and `session.report` under "Execution
   diagnostics". The alternative, `record.producer`, names a prefix no other kind uses.
2. **Where it is emitted -- Recommended: `Lifecycle::starting`**, ahead of `starting`. The
   alternative, `open_stream`, leaves the test path that hands the builder a stream without it,
   and `start_on_host` would carry it separately.
3. **`Session::producer()` beside the record -- Recommended: yes.** A reader that lets 4,096
   events queue loses the oldest, the first record among them (`_host.py:579-610`), and a program
   that never reads `events()` still needs the version for its own records. The alternative, the
   stream alone, costs nothing now and that reader later.
4. **The bound on an unknown type's detail -- Recommended: 4 KiB** of the `data`'s JSON text, with
   the omitted byte count said. Larger shows more of a payload nobody interprets; smaller hides a
   field a reader might recognize. Open.
5. **The renderer's fixture from the writer -- Open; recommended: not now.**
   `plan/next/render-session-fixture-from-the-writer.md` would have `events_tests.rs` write the
   fixture `every_event()` is today, so a new kind or field fails the sweep until the page shows
   it. This task adds its kind by hand, as every task has; the entry stays.

## Dependencies

- **Hard: `0003-19`.** The stream, the subscription and the file writer.
- **Hard: `0003-14`.** The renderer and its test.
- **Hard: `0003-29`.** The host program's `Session`, `events()` and the `session.start` answer.

## See also

- `plan/phase/0003-python/diagnostics.md` -- "Records name their producer", "A bounded
  interface for a reader".
- `plan/phase/0003-python/observability.md` -- "The envelope is CloudEvents; the payloads are
  OutRig's", "The renderer".
- `plan/phase/0003-python/embedding.md` -- "A Python program as owner", the events bullet.
- `doc/reference/events.md`; `scripts/render-session.py`; `scripts/render-session-test.py`.
- `/home/travis/Downloads/outrig-cococlaw-planning/0008/agent-debugging-ui-needs.md` -- "Safe raw
  inspection and evolving compatibility".
