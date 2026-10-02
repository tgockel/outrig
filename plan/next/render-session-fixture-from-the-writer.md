# The renderer's every-event fixture comes from the code that writes events

## Context

`scripts/render-session-test.py` (`0003-14`) sweeps a hostile payload through every field of
every event type and asserts each field is either on the page, escaped, or named in `NOT_SHOWN`
with a reason. The fixture it sweeps, `every_event()`, is written by hand. A test checks its event
*names* against `crates/outrig/src/events.rs` and `doc/reference/events.md`, so a new event fails
the run, but a new *field* on an existing event does not: the sweep never plants it, so it can
be missing from the page and nothing notices.

`events_tests.rs`'s `each_event_has_exactly_its_data_fields` already constructs every event type
with every field.

## Shape

- That Rust test writes, or compares against, a checked-in `scripts/fixtures/every-event.jsonl`
  holding one of each event, the way `public-api.txt` is a checked-in record of the surface.
- `render-session-test.py` sweeps that file instead of `every_event()`.

## Acceptance

- Adding a field to an `Event` variant fails `cargo test` until the fixture is regenerated, and
  then fails the renderer's sweep until the page shows the field or `NOT_SHOWN` says why not.
