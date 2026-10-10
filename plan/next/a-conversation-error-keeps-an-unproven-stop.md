# A conversation that fails still exits 3 when the stop is unproven

## Context

`run-new` maps the shutdown report to its exit status only when the conversation ended cleanly.
When `converse` returns an error -- a `BrokenPipe` writing to a stdout piped into a reader that
went away, say -- `run_new.rs` skips `exit_status`, and both the process and `session.json` exit 1,
even when the report reads not proven stopped. Status 3 is the one that says something may still
be running, and it is meant to outrank everything. This predates `0003-19`: before it, an error in
the conversation also exited 1, and the cleanup's failures were not reported at all.

## Shape

- Keep the error's diagnostic as `app.rs` prints it, but let a `NotProvenStopped` verdict make the
  status 3; every other error keeps 1.
- `app.rs` turns an `Err` into status 1 after printing it, so the error needs a way to carry its
  status there -- a `CliError` variant that wraps another with a status, say -- rather than
  `run_new.rs` printing the error itself in `app.rs`'s format.

## Acceptance

- A unit test at the point `run-new` maps a report and a conversation's result to a status: an
  error with a not-proven-stopped report gives 3 and the error's text; with any other verdict, 1.
