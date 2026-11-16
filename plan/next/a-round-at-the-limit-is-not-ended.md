# A round the model-request limit refused reads as a round that ended

## Symptom

At the primary agent's model-request limit, `Session::round` opens no round and returns
`RoundOutcome::Ended` with the limit's stop and an empty reply, as `model-policy.md` ("The
primary at the limit") and `0003-50` specify. Yet `RoundOutcome::Ended` and `RoundEnd` both say
a round ran. An owner whose loop asks again after every `Ended` until `NothingNew` -- what
`run-new` did, so a message arriving after a round's last result is told -- spins at the limit:
the message waits unannounced, so each call answers `Ended(limit)` again. `run-new` now checks
the stop's kind (`converse.rs`, `asks_again`); every other embedder has to know to do the same.
Found by `0003-50`'s altitude review.

## Shape

`RoundOutcome` is `#[non_exhaustive]` and 0.3.0 is unreleased (`embedding.md`, "Stable at
0.3.0, not before"), so an outcome of its own -- `NotOpened(RoundStop)`, say, or one carrying
the `ModelRequests` read -- says what happened, and a loop that asks again only after `Ended`
stops on it with no predicate. `model-policy.md`, "The primary at the limit", changes with it,
and `asks_again` goes. That path's tasks query, which waits up to `TASKS_TIMEOUT` for a round
that never ran, could go too.
