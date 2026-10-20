# A host program's sessions are listed with the rest

## Context

A session a host program enters (`0003-29`) has its directory under the session root,
`<session-root>/<id>/logs`, where `events.jsonl` goes under `[events] mode = "record"` and the
container writes its logs. The library makes that directory; `session.json`, which `outrig ls`,
`logs`, `discard` and `clean` read, is the CLI's `SessionStore`, and the library writes none:
`harness-components.md` keeps session records out of it. So such a session is on disk but in no
listing, `outrig logs` cannot find it, `render-session.py` renders it with a note that there is
no `session.json`, and `outrig clean` cannot report its container as a stray after `outrig` itself
was killed.

## Shape

- The engine tells its owner each session it starts for the program and each it ends -- the id,
  the container's name, the image's tag, and the exit's verdict -- through `RunningProgram`, as
  values the CLI reads; no hook into the library's session start.
- The CLI writes `session.json` when one starts, with the program's working directory as its
  `working_dir` and the image-config it ran, and finalizes it when it ends, its `exit_code` the
  status the report decides as `run-new`'s does.

## Acceptance

- A program's session is listed by `outrig ls` while it runs and after it ends, `outrig logs` and
  `render-session.py` read its directory, and `outrig clean` finds its container after `outrig`
  was killed with `SIGKILL`.
