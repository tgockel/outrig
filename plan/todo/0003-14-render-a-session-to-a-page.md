# 0003-14 -- A script renders a session directory to one page

## Context

`events.jsonl` is legible to `jq` and to nobody else. `observability.md` pairs it with a single
script that reads a session *directory* -- `session.json` for what the session was,
`events.jsonl` for the timeline, `network.jsonl` when it is there -- and writes one
self-contained HTML file.

Deliberately minimal: a timeline, per-agent REPL history with tracebacks, the messages, and a
token summary. Nothing interactive, no server.

Two details the page settles. Dependencies are declared inline and `uv` provisions them -- the
standard-library-only rule `scripts/audit-doc-style.py` states for itself does not transfer,
because that script is a CI gate and this one is run by a person. And autoescaping must be
**enabled**: Jinja's `Environment` defaults it to `False`, so a template engine is not safe by
having been imported.

## Goal

Someone who ran a session can look at what happened in a browser, without installing anything
first.

## Deliverables

- The script, with a PEP 723 block declaring its dependencies and pinning the interpreter version.
  Invoked as `uv run --script`, not `uvx` -- `uvx` runs a command from a published package.
- Reads the session directory, not a single file, so metadata and the network audit are available
  without a second invocation.
- **Autoescaping explicitly enabled**, with the hostile inputs the record is made of exercised:
  submitted source, tracebacks, captured output, and message bodies.
- A timeline correlating a round to the executions and messages inside it, using the causal ids
  the events carry.
- A token summary distinguishing per-round totals from peak per-request context use.
- Few dependencies. The inline block is an inventory a reader can check before running something
  over their own session, and it is worth keeping short enough that they do.

## Acceptance

- Run against a real session directory produced by `0003-13`, it writes a page that opens.
- **Hostile input is escaped**, asserted per field: a `<script>` tag in submitted source, in a
  traceback, in captured output, and in a message body. Four assertions, because they reach the
  page by different paths.
- A session directory missing `network.jsonl` renders without it rather than failing.
- A truncated final line -- the record was being written when the session died -- does not abort
  the render.
- The script runs with no prior install, on a machine with only `uv`.
- **The escaping assertions have a CI home**, with synthetic source, output, traceback, and
  message fixtures. A render that happened to be safe once is not regression protection, and this
  is the one output in the phase that a browser executes.
- `python3 scripts/audit-doc-style.py` still passes; the tree gains an entry in
  `.claude/CLAUDE.md`'s `scripts/` description.

## Design forks

1. **Where it lives -- Open.** `scripts/` is described in the tree as repo-local tooling, and this
   is the first thing there meant for a user. Either move the description or find it a home.

## Dependencies

- **Hard: 0003-13.** There is nothing to render until a session writes events.

## See also

- `plan/phase/0003-python/observability.md` -- the renderer section, and the autoescape correction.
- `scripts/audit-doc-style.py` -- the stdlib-only policy and the reason it does not transfer.
