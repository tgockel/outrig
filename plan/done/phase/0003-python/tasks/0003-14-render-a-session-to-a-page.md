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

## Decisions

- **Fork 1: it lives in `scripts/` (the maintainer's call).** `scripts/render-session.py`, as the
  design page drew it. The tree's description of `scripts/` now says it holds this one script meant
  for users. It is in neither crate's package, so the usage docs tell a `cargo install` user to take
  the copy from the `outrig-cli-vX.Y.Z` tag matching their binary.
- **"Pinning the interpreter version" is read as a minimum.** `requires-python = ">=3.12"` and
  `jinja2>=3.1`, with no exact pins and no lockfile beside the script. An exact interpreter pin
  would make `uv` download a Python on machines that already have a newer one, for no gain the
  script can name; a `.py.lock` would be a second file to keep in step. Jinja's only transitive
  dependency is MarkupSafe, so the inventory a reader checks is two packages.
- **The page goes to `<session-dir>/report.html` by default, owner-only.** Not the current
  directory: `run-new` is run from inside a repo, where a page holding the conversation is one
  `git add -A` from a commit. Beside `events.jsonl` it has that file's sensitivity and lifetime,
  and `outrig discard` removes it. It is written to a `mkstemp` file and renamed into place, so it
  is `0600` whatever the umask, an older and wider page there is replaced rather than reused, and
  a symlink at the path is replaced rather than followed. An `--out` naming one of the inputs is
  refused.
- **Escaping has two layers, and the tests are about the first.** Autoescaping is on, and data
  reaches the page only as text or a quoted attribute; anchors and links are built in Python from
  integer ids alone, so a hostile id gets no anchor rather than an odd one. A
  Content-Security-Policy `<meta>` (`default-src 'none'`, inline style only) is the second layer.
  The page has no script or image of its own, so the tests can assert that none appears anywhere.
- **The CI home (the maintainer's call): `scripts/render-session-test.py`, through `uv`.** Standard
  library only, and each case runs `uv run --script scripts/render-session.py` as a subprocess:
  the claim under test is the script as a person runs it, PEP 723 block and all, not an import of
  it. The `render-session` job installs only uv and sets `UV_PYTHON_PREFERENCE=only-managed`, so
  the runner's own Python is never used. That job is the evidence for "runs with only `uv`".
  - setup-uv is pinned to `v10.2.0`: it publishes no moving `v10` tag. Its cache is off, because
    it keys on lockfiles and `pyproject.toml`s and this repo has none.
- **Four escaping tests, then a sweep that cannot pass by omission.**
  - The four plant `<script>` in submitted source, a traceback, captured output, and a message
    body. Each asserts the escaped text is on the page as well as that no raw tag is, so a field
    that silently stopped rendering fails rather than passes.
  - The sweep plants a distinct payload in every string field of every event type and of a
    `network.jsonl` record, whose `outrig.host` and SNI the container can assert. Every marker
    must appear escaped, unless the test's `NOT_SHOWN` list names the field and says why it is not
    shown. The tags rig's turn JSON is told apart by, `role` and `type`, are left as they are, so
    each turn item is read the way a real one would be. A second pass plants the payload in every
    field, tags, numbers, and booleans included, which drives the coercion helpers.
  - Autoescaping turned off fails six tests; that was checked once by hand.
  - The sweep's fixture is compared with the event names `events.rs` writes and `events.md`
    documents. An event `0003-15` or later adds fails the run until the renderer has been looked
    at. `model.retry` and `model.failover` already render, so `0003-15` should not need to touch
    it.
- **Rounds are keyed by when they started, not by number.** A round that commits no turn leaves its
  number to the next, which the real session showed: a 500 failed round 2, and the next round was
  round 2 again. They render as `Round 2` and `Round 2, attempt 2`. Events outside a round go in
  blocks of their own, and a round the record ends inside is shown as unfinished.
- **Correlation uses the ids the events carry, and claims nothing the record does not.**
  - Executions link by `execid` and messages by `message`, globally, since an `exec.completed`, a
    probe, or a `message.received` can land rounds after what it is about. A late result says
    which round it was submitted in.
  - A turn's tool call links to an execution only when its source matches an `exec.submitted` in
    the same round exactly. Matching by position would assert which call produced which
    execution, and the record does not say. A tool call that matches nothing -- a refused
    submission -- shows its source inline, the only place it exists.
  - A round's opening shows in its first turn, and folded under `model.call`, where it is the only
    record when the round commits no turn.
- **Tokens keep what a round cost apart from what one request carried.** Per round: every usage
  field as reported, never recomputed, summed over `calls` for a failed or dropped round, which
  has no sum of its own. Beside them, the largest single request's reported input, OutRig's
  largest estimate, and the window that request was held to. The estimate is the one peak that
  compares across providers, since Anthropic's input excludes cache reads and OpenAI's includes
  them; the page says so.
- **A record that cannot be trusted whole still renders, and says why.** Lines that do not parse
  are listed, including a final line cut inside a UTF-8 character, which is why lines are decoded
  one at a time. The page also reports gaps in ids, a record that does not end with
  `agent.stopped`, joined logs, and `session.json` naming a different container from the agent's.
  An empty log renders, a missing `session.json` or `network.jsonl` is a note or an absent section,
  and only a missing `events.jsonl` fails, with the `[events]` setting named.
- **An event's agent is shown on its row only when the record holds more than one.** One agent runs
  today; the executions are grouped by `subject` regardless.
- **What the review of the schema found went into `events.md`.** Execution and message ids share
  one counter, with gaps; `adjacent[].turn` is `null` at the round's opening; `exec.refused` has no
  `exec.submitted` or `exec.completed`; and a turn joins after the executions it ran.
- **Verified against a real session.** `outrig run-new` ran under podman with a throwaway stand-in
  for Anthropic's API on the host, and with `[events] mode = "record"` and `[network] mode =
  "audit"`. The session covered:
  - `<script>` in submitted source, in printed output, in a raised exception, in a typed line, and
    in the agent's send;
  - a 500 that failed a round, so its number was reused;
  - a Ctrl-C on a `time.sleep`, recording a cancel, a failed liveness check, and an interrupt;
  - one audited connection, with a container-asserted host.

  The page rendered under headless Chromium with every one of those as text, and the file was
  `0600`. A copy of the log truncated mid-record rendered too, listing the cut line, the
  unfinished round, and the missing `agent.stopped`.
- **What the review pass changed, and what it filed.**
  - Each table's columns are one spec in Python (`USAGE`, `TOKEN_COLUMNS`, `NETWORK_COLUMNS`),
    and a round's usage lives on its timeline block, so the session row is one more block.
  - A run of `output.unattributed` lines is joined once at the end. Appending to one string per
    line was quadratic, and the comment beside it expected thousands of lines.
  - Gaps in ids are found by walking the ids rather than by building every id up to the largest,
    which one hostile id would have turned into the whole of memory. An id is a number only when
    `str.isdecimal` says so: `isdigit` accepts `"²"`, which `int` rejects.
  - Filed rather than done, since each changes what `events.rs` writes:
    `plan/next/exec-events-name-their-tool-call.md`, which would replace the source match;
    `plan/next/every-round-ending-carries-its-usage.md`, which would retire the renderer's
    rebuilding of a failed round's sum; and `plan/next/render-session-fixture-from-the-writer.md`,
    which would let the sweep catch a new field as it catches a new event.
- **A flaky test from `0003-13`, fixed in a commit of its own (the maintainer's call).**
  - `with_events_off_no_log_is_written` failed about one full `cargo test` run in three, and never
    alone. It closes an event log and opens it again, expecting "already holds a recording", and
    instead got "already owned by another agent".
  - Measured, the lock outlived `close()` by 0.3 to 0.6 ms. At the moment of the refusal no
    descriptor in the process had the log open, so the writer had let go.
  - The failure came only with process-spawning tests running alongside: two in 15 runs beside the
    `python::` tests, none in 15 with one test thread.
  - The account that fits: `flock` belongs to the open file description, and a process being
    spawned holds a copy of the parent's descriptor table until it execs. A spawn on another test
    thread therefore keeps a just-closed log locked for that long. A per-process fcntl lock would
    not have this property, but would also stop refusing a second writer in the same process,
    which is what the lock is for. So the tests wait instead of the sink changing.
  - The new helper `events::released` takes a blocking lock on a fresh descriptor, and both tests
    that reopen a log call it before reopening. The other test,
    `a_log_that_holds_a_recording_is_refused_and_kept`, had the same race and had not yet failed
    here.
  - `cargo test -p outrig --lib` then passed 20 runs in a row.
- **A review of damaged and crafted records, after landing.** Six cases, each reproduced first
  and each now with a test that fails on the code before the fix:
  - A tool call whose submission line was lost was labelled "not run". Failing to find a
    submission does not prove nothing ran, so the label says only that none was recorded.
  - A model call whose `round.started` was lost was counted outside any round, and the round its
    ending opened showed no calls. A model call now opens a round marked as having no recorded
    start, from its own `round`.
  - A record with no gaps was summed up as "Nothing missing". An event the queue had no room
    for spends no id, so the page now says what it checked, and that caveat is unconditional.
  - `1e309` and `NaN` are valid JSON, and an infinite duration aborted the render. Numbers are now
    what the schema writes, finite and within a u64, or are shown as text.
  - An envelope id of 4,301 digits passed `isdecimal` and then failed `int`. Ids past a u64's
    length are now treated as not numbers.
  - A `session.json` nested 10,000 deep raised `RecursionError` on 3.12 and 3.13, which the
    handler did not catch; on 3.14 it parsed as a list and was silently ignored. Both now say it
    could not be read. CI runs the suite on 3.12 as well, since that is the only place the first
    of those is reachable.
- **A second review pass, the same way: two more, each reproduced and tested.**
  - With every execution record lost, the page said "Nothing was submitted" beside a timeline
    showing the call. It now says no executions were recorded.
  - With round 1's ending and round 2's start both lost, round 2's call and tokens were credited
    to round 1, shown as completed. A model call or round ending that names a different round
    from the open one now closes it as unfinished and opens a marked round. That replaced a
    health note that named the mismatch and left the totals wrong. The same number twice infers
    no boundary, since a round that commits no turn leaves its number to the next.
