# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **The built-in default image-config keeps its `view = "primary"` servers on a build without
  the musl target.** Such a build used to have no `outrig-enter` launcher, so `fs` fell back to a
  read-write bind mount of the workspace and `shell` was dropped. The `outrig` crate now builds
  the launcher statically for the build machine's own target when the musl target is missing, so
  both servers run as they do on a musl build, and they degrade only when that build fails too.
  The note printed then links https://tgockel.github.io/outrig/usage/no-musl.html rather than a
  `doc/` path an installed binary does not have. (#868)

### Fixed

- **A panic out of a session's root future no longer hangs `outrig` on stdin, ignoring SIGINT and
  SIGTERM.** `block_on_session` shuts its runtime down with `shutdown_background()` rather than
  an ordinary drop, because a REPL's or a stdio MCP transport's blocking stdin read can still be
  in flight and `Runtime::drop` waits for it with no timeout. That call ran only on the return
  path: a panic unwinding out of the session future skipped it and dropped the `Runtime` the
  ordinary way, so the process sat on the blocking read after the panic was reported, and tokio's
  signal handlers, installed but orphaned once the runtime they report to was gone, no longer
  ended it either -- only the other end of the pipe closing, or SIGKILL, did. The runtime now
  lives in a guard whose `Drop` calls `shutdown_background()`, so the unwind path gets the same
  treatment as the return path and the process exits promptly either way. No reachable panic site
  is known yet; this closes the gap before one is found. (#517)
- **`outrig image add`'s bootstrap no longer reads a `--global-config` that vanished while its
  "Configure outrig in this directory now?" prompt was up as an empty config.**
  `resolve_or_bootstrap` checked an explicit path once, before that prompt, and read the file
  after -- so a file removed in the window the prompt paces was read as missing and silently
  treated as an empty global config, same as #342's unconfigured case, and the wizard wrote a
  repo config whose agent has no model without saying why. The explicit path is now re-resolved a
  second time, right before the bootstrap reads it, so a file gone by then is refused with the
  same "--global-config <path> does not exist" error as one missing from the start; a path
  already missing before the prompt is still refused ahead of it, so a scripted run with nothing
  scripted for that prompt fails on the missing file rather than hanging on the question. `run`,
  `mcp`, `build`, and the session commands check and read the file in one pass with no prompt
  between, so they were never exposed. (#505)
- **`outrig clean` and `outrig discard` no longer delete the record of a session whose container
  started while the `[y/N]` prompt was open.** Both commands decided what was removable from a
  single liveness check taken before the prompt -- `owner_is_live` for the session's directory, or
  its container among a `podman ps -a` taken at the same time -- and removed unconditionally once
  the user answered, however long that took. A container started in that window (by `podman
  start`, or another tool) went unnoticed, and its record, `logs/`, and `outrig-enter` were
  deleted and reported as an ordinary removal, though the docs and `clean`'s own module comment
  said this never happens. Both commands now look again immediately before each record's removal:
  `clean` keeps the record and reports it the way a session the preview itself caught is kept,
  without failing the sweep; `discard` refuses with the same error it already gives before its
  prompt. The second look narrows the window between the check and the removal to the time
  between that look and the removal itself; it cannot close it. `clean`'s stray-container
  sweep is scoped the same way: a sidecar labeled with a session the second look retained is
  no longer carried into the `podman rm` batch that sweeps the containers of records
  actually removed this run. (#869)
- **`outrig clean` no longer stops and deletes a stray container that started while its prompt
  was open.** It chose strays from a `podman ps -a` taken before the prompt and then removed them
  with `podman rm -f`, which kills a running container, so one revived in that window was lost and
  reported as an ordinary removal. The removal is no longer forced: podman refuses a running
  container when it acts, and `clean` reports `kept container <name>: it is running or paused`
  and still exits `0`; a paused container counts as live. A removal that fails for another reason
  now prints podman's explanation. (#482)
