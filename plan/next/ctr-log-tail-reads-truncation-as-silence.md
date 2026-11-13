# `ctr_log::stderr_text` reads a tail, and an empty parse is reported as silence

`stderr_text` (`crates/outrig/src/container/ctr_log.rs`) reads the last `TAIL` (64 KiB) of
a container's conmon log and parses the `stderr` entries out of that window. A container
that writes its diagnostic early and then more than 64 KiB of protocol on stdout has the
diagnostic in the log and outside the window, so the parse comes back empty.

`restore_lost_stderr` (`crates/outrig/src/mcp.rs`) reads that empty parse as "the container
wrote no stderr" -- it logs "neither podman's attach stream nor the container's log holds
any stderr", restores nothing, and returns `LogLookup::ConfirmedSilent`. Since #727 that
answer also suppresses the `note:` line qualifying an empty `<server>.stderr`, so a
truncated window now reads as a positive claim that the server was silent rather than as a
window that could not see.

Pre-existing: the bounded read and the early return are both older than #727, which only
gave the classification a second consumer.

## Shape of a fix

- Have `stderr_text` report whether its window reached the start of the log -- a struct, or
  a `(String, bool)` -- instead of only the text. `start == 0` in the existing seek is
  exactly that bit.
- `restore_lost_stderr` returns `ConfirmedSilent` only for an empty parse of a whole log,
  and `Inconclusive` for an empty parse of a truncated one, so the note is kept.
- Optionally widen the recovery: scan backwards past the window for the most recent `stderr`
  entries rather than giving up on the first 64 KiB, which is what would actually restore
  an early diagnostic.

## Acceptance

- A log whose only stderr entry sits before the window leaves the failure qualified rather
  than reported as silence.
- A log read in full that holds no stderr still reports silence, with no note.
- Unit tests for both, alongside `ctr_log`'s existing tail tests.
