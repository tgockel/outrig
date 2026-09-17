# 0021 -- Session CLI (`ls`, `logs`, `discard`)

## Goal

Implement the three session-management subcommands using the `SessionStore` from 0020.

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

- 0020-session-store

## Notes

- Tabular output: hand-roll with `println!`-and-padding rather than pulling in a table crate
  -- it's one place, and the column widths are predictable.
- `--follow` rotation handling can be tightened later; v0 just needs `tail -F`-equivalent for
  a file that mostly grows.
- Substring matching on `<session>` (per the docs): if the substring matches >1 session,
  error with a list of the matches.

## Decisions

- **`AsyncWrite + Unpin` generic streams, not `&mut dyn Write`.** Mirrors the existing
  `Repl::run_with` pattern at `src/repl.rs:83-96`; `tests/repl_io.rs` already wires
  `tokio::io::duplex` halves the same way, so `tests/session_cli.rs` reuses that idiom
  rather than introducing a second test-injection style. All three subcommands are async
  for dispatch uniformity even though `ls` doesn't strictly need it.
- **Standalone session-root resolver (`session::resolve_session_root_for_cli`).**
  `Config::load` requires a repo's `config.toml` to exist (`src/config/mod.rs:55-58`);
  the inspection commands must work outside any repo. The new resolver tries the repo
  config (silently skipping `NoRepoConfig`), then the global config, then the XDG default
  -- and skips full validation since we only need the `session-root` key.
- **Substring `<session>` resolution lives in `src/cli/mod.rs`, not on `SessionStore`.**
  CLI ergonomics policy, not a store invariant. Keeps the 0020-locked store surface
  minimal; if a non-CLI consumer ever appears, it gets exact-match by default.
- **`SessionStore::symlink_path(&id)` instead of a leaky `root()` accessor.**
  `discard` needs the `<root>/<sid>` path for the user-facing "removed ... (symlink)"
  message. Exposing `root()` would leak storage layout; a named accessor keeps the
  store responsible for the "where does the symlink live" question.
- **`is_running` closure takes `String`, not `&str`.** Avoids the HRTB friction with
  `FnOnce(&'a str) -> impl Future + 'a` -- a single `.clone()` on the container name
  per discard call is negligible cost. Production passes a closure that runs
  `podman ps -q --filter name=^<name>$`; tests pass `|_| async { Ok(false) }` (or
  `Ok(true)` for the refusal path).
- **`discard --session-dir` short-circuits root resolution.** Resolving the session
  root would do unnecessary config reads -- and worse, would surface a
  "malformed config" error on a path that doesn't need the config at all.
- **`--follow` is a polling loop with size-based rotation detection, not `notify`.**
  `notify` isn't in deps; the task explicitly authorizes polling. Open the file once,
  cat existing content, then loop: 200ms sleep -> stat -> if size grew read+print
  delta, if size shrank reopen+reset position. Terminates on `tokio::signal::ctrl_c`.
  Atomic writes that don't observably shrink the file are an accepted v0 limitation.
- **`--follow` errors immediately on missing files, no wait.** Easier to debug a typo
  on the server name than to silently hang; the doc's example assumes the file already
  exists (the session is running).
- **Missing repo config (`NoRepoConfig`) is silently skipped, not propagated.** The
  inspection commands read at most an XDG default; a repo-less invocation is a
  legitimate use case, not an error.
- **Test fixture lives at `tests/common/mod.rs` (cargo's standard convention).**
  `sample_session` was duplicated between `tests/session_store.rs` and the new
  `tests/session_cli.rs`. Moving to `tests/common/mod.rs` (subdirectory + `mod.rs`,
  not a top-level `common.rs`) avoids cargo treating it as its own test binary.
