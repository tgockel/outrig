# 0003-49 -- The owner reads the roster

## Context

The children's registry holds every agent of a session's tree under one lock: `State::agents`
maps each id to a `Node` with its parent, depth, model, launched children and, for a child, a
`ChildNode` with its `Life` -- launching, running, idle, wedged, released -- whether it is
resident, its runner, the round it is in and what was delivered on each channel
(`crates/outrig/src/agent/children.rs:78-134`); `State::requests` holds every request made of a
child (`:86-87`, `:147-167`). The primary's node is made by `Children::root` at depth 1
(`:251-264`); `resident` counts what `children-max` counts (`:304-308`); `summary`, `counts` and
`open` read a child's unread and taken requests (`:1012-1037`, `:1041`, `:1101-1108`). The
registry tells a child's parent kernel the word for its life, and no one else (`tell_life`,
`:1175-1194`); the report lists each request open at the close and how it ended
(`Children::report`, `:266-287`; `crates/outrig/src/harness/report.rs:113-126`). `0003-48` adds to
that state each agent's stop-read state -- unavailable with its next attempt, failed, the last
state at rest -- and a sequence beside the lock.

OR-A6-3 asks for a live roster the owner can query authoritatively: id, class or kind, name
where one exists, parent, depth, state including retrying and limit-reached, the last stop, the
model in use and, for a child, its outstanding requests; a read, not a subscription, lagging by at
most one lock, agreeing with the shutdown report at the end. It serves the application's
heartbeat roster and the runtime facts of assessment, which `continuation.md`, "Assessment",
reads from this snapshot (`0003-60`). OR-A4-3 asks that a child at its model-request limit show
so (`0003-50`). `rust-owner.md`, "The roster", records the design.

`0003-50`, which follows, adds the limit-reached state the row shows; `0003-60` reads this roster.

## Goal

The owner reads a snapshot of the session's agents -- each with its id, class or kind, name,
parent, depth, state, last stop, model and outstanding requests -- as a read under the
registry's lock that agrees with the shutdown report at the end.

## Deliverables

- **`Session::roster() -> Roster`**, with `seq`, the registry's change sequence at the read, and one
  `AgentRow` per agent in the tree: `id`; `kind` -- the primary, a root instance with its class as
  `module.qualname`, a request child with its class when it is an instance's, or a work child;
  `name` where the spawn gave one; `parent`, the owner or an agent's id; `depth`; `state` in
  `0003-48`'s terms plus `wedged` and `releasing` (fork 2); `last_stop`, the last state at rest, or
  none; `model`, as named; and `outstanding { unread, queued, taken }` for a child, projected from
  every non-ended record the registry holds for it -- `summary` is the scheduling view, which keeps
  one submission for a work child and drops the queued ones, and the roster is not derived from it.
  Spellings are this task's. Public API additions; `crates/outrig/public-api.txt`.
- **A read under the lock.** The snapshot is assembled in one critical section of the registry's
  lock, so it lags any change by at most one lock and never mixes two moments; it awaits nothing,
  the interpreter included.
- **Which agents it lists** (fork 1): every agent resident or launching, and a released agent
  until its kernel is gone; a `no_primary` session with no root lists nothing.
- **Agreement with the report.** A roster taken after admission closed, when nothing more can be
  requested, lists outstanding exactly the non-ended records the report's `requests` lists at the
  close -- queued submissions included -- and nothing the report does not: both are projections of
  the same registry at the same snapshot, and the report is the last word, since it is read after
  the roster can be.
- **`0003-50`'s state** shows as the row's `state` once that task lands; `0003-60` reads this
  roster for its agent-level facts (`continuation.md`), and nothing here is built for it beyond
  the read.
- **Events and the close table.** No new event and no close row: a read closes nothing and
  publishes nothing. The task's `## Decisions` says so.
- **`## Decisions` records**: forks 1 and 2; the row's spelling; and that the roster carries no
  token usage, with the open question noted (`rust-owner.md`).

## Acceptance

Each is a Rust test with a mock model, against a root `0003-46` constructs.

- A root with two children, one of them retrying after 500s from the mock: three rows; the
  root's row names its class, `owner` as parent and depth 1; the retrying child's `state` is
  `unavailable` with `next_attempt` between two and sixteen seconds out; after releasing one
  child the roster has two rows, and the released child's row is gone once its kernel is gone.
- A child wedged in `while True: pass` shows `wedged` and stays listed, and the resident count
  equals what `children-max` counts.
- Outstanding requests: a request child with one unread and one taken request shows
  `{unread: 1, queued: 0, taken: 1}`; after a reply, `{1, 0, 0}`; after a release, the row is gone.
  A work child with one taken submission and two queued shows `{0, 2, 1}`, where `summary` would
  report the taken one alone.
- The roster taken immediately after `agent.spawned` is observed on a subscription includes the
  child: one lock's lag at most.
- With no subscriber, and with a subscription never read, every read above is the same.
- Agreement: with a work child holding one taken and two queued submissions and a request child
  holding one unread, close admission, read the roster, then shut down: every one of the four the
  roster lists outstanding is in `ShutdownReport::requests`, ended `closed`, and nothing else is.
- A `no_primary` session with no root has an empty roster; a session with a primary agent lists
  the primary at depth 1 with `parent` none.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Released agents -- Recommended: listed as `released` until the kernel is gone, then
   dropped.** `children-max` counts a resident child until its kernel ends (`children.rs:117-118`),
   and an owner reading the roster for capacity wants the same count. The alternative drops a row
   at release, which undercounts what the session still holds.
2. **A `releasing` state -- Recommended: shown**, between the release's start and the kernel's
   end (`WEDGE_WAIT`, `:45-47`), since an owner that sees `released` and then a still-resident row
   would otherwise read a contradiction. The alternative folds it into `released`.

## Dependencies

- **Hard: `0003-48`.** The state and the sequence each row reads, and the handle the root is
  read through.
- **Hard: `0003-30`.** The registry and the request machinery.
- **Soft: `0003-46`.** The root the acceptance constructs.

## See also

- `plan/phase/0003-python/rust-owner.md` -- "The roster".
- `plan/phase/0003-python/continuation.md` -- "Assessment".
- `plan/phase/0003-python/work.md` -- "Limits belong outside generated code", for what
  `children-max` counts.
- `crates/outrig/src/agent/children.rs`, `crates/outrig/src/harness/report.rs`, `session.rs`.
