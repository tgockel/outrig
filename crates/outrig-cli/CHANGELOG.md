# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

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
