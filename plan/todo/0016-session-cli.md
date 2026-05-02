# 0016 -- Session CLI (`ls`, `logs`, `discard`)

## Goal

Implement the three session-management subcommands using the `SessionStore` from 0015.

## Deliverables

- `outrig ls [--session-root <path>]`:
  - Newest-first table with columns matching `doc/usage/sessions.md`: `ID`, `STARTED`,
    `DURATION`, `CONTAINER`, `EXIT`.
  - Symlinked rows get a `-> <target>` suffix on the EXIT column (or as a trailing column,
    designer's call -- the doc shows it appended to the row).
- `outrig logs [<session>] [<server>] [--follow] [--session-dir <path>] [--session-root
  <path>]`:
  - `<session>` and `--session-dir` mutually exclusive (clap-derive `ArgGroup` with
    `multiple=false`).
  - With no `<server>`: list available log files with sizes (`fs (1.2 KiB)` etc.).
  - With a `<server>`: cat the file to stdout.
  - `--follow`: `tail -F`-style behavior; rotate-aware via `notify` or simply re-stat. v0 can
    do a simple polling loop with `tokio::fs::File::seek` + 200 ms sleep.
- `outrig discard [<session>] [--yes] [--session-dir <path>] [--session-root <path>]`:
  - `<session>` and `--session-dir` mutually exclusive.
  - Without `--yes`: print the dir to be removed and prompt `Discard? [y/N]:`.
  - Refuse if the session's container is still running (`podman ps --filter name=<container>`
    returns non-empty); error with a pointer at the running container.
  - On accept: `SessionStore::remove_by_id` or `remove_by_path`.
- `tests/session_cli.rs` over a synthetic root directory (no podman dependency for `ls`/`logs`
  unit tests).
- An integration test for `discard` that bypasses the running-container check (mock or
  fixture).

## Acceptance

- After two `outrig run` invocations, `outrig ls` shows both newest-first.
- `outrig logs <sid> fs` cats the captured stderr; `--follow` streams new writes.
- `outrig discard <sid> --yes` removes the dir; for symlinked entries, removes both the target
  and the symlink.
- Drop the `> TODO: Incomplete` marker on `doc/usage/sessions.md`.

## Dependencies

- 0015-session-store

## Notes

- Tabular output: hand-roll with `println!`-and-padding rather than pulling in a table crate
  -- it's one place, and the column widths are predictable.
- `--follow` rotation handling can be tightened later; v0 just needs `tail -F`-equivalent for
  a file that mostly grows.
- Substring matching on `<session>` (per the docs): if the substring matches >1 session,
  error with a list of the matches.
