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

## Decisions

Forks 1 and 2 were taken as recommended; the primary's row is the maintainer's choice (below).

- **The spellings.** `Session::roster() -> Roster { seq, agents }`, in a new
  `harness::roster` module; `AgentRow { id, kind, name, parent, depth, state, last_stop, model,
  outstanding }`; `AgentKind::{Primary, Root { class }, Request { class }, Work}`;
  `Parent::{Owner, Agent(id)}`; `RosterState::{Agent(AgentState), Releasing, Wedged}`;
  `Outstanding { unread, queued, taken }`. Every type and struct variant is `#[non_exhaustive]`.
  `RosterState` wraps the stop read's `AgentState` rather than adding `Releasing` and `Wedged` to
  it, since the stop read never says either; `0003-50`'s `limit reached` shows through
  `RosterState::Agent` once it is an `AgentState`.
- **The primary agent's row has no state (the maintainer's choice).** `state`, `last_stop` and
  `outstanding` are `Option`s, `None` for the primary and always `Some` for a child. The registry
  keeps no stop state for an agent its owner drives: its rounds are `Session::round`'s, whose
  result is each outcome, and `roster(&self)` cannot be called while that `&mut self` round runs,
  so a state read there would only ever say idle. The alternatives -- a `Driven` variant, or
  running and idle read from its call slot -- were declined.
- **Fork 1: listed while resident.** A child is listed from its launch until its kernel is gone,
  which is `ChildNode::resident`, what `children-max` counts; a launch that fails removes its node,
  and its row with it. The program that owns the session is the tree's root at depth 0, no agent,
  and never listed. A `no_primary` session with no root lists nothing.
- **Fork 2: `releasing`.** A listed child whose stop state is `Released` or `Cancelled` reads
  `Releasing`; one whose kernel did not end within `WEDGE_WAIT` reads `Wedged`, the one fact only
  `Life` holds; once its kernel is gone it is dropped. A launch that fails now says `Cancelled`
  too, as a release while launching does, where it left the stop state `Launching` until its node
  was removed, so the row reads the release from the stop state alone. So a row's state never says `released`:
  that is the stop read's word once the row is gone, and the row's `last_stop` carries it, or
  `Cancelled`, meanwhile. The close's release reads the same, from the close until the session's
  end ends the kernels. An interpreter that died leaves each child `Agent(Terminated)` and
  listed: nothing tells the registry each kernel is gone, and the rows say what happened.
- **`last_stop` is the latest state at rest the child came to, the one it is in included**, kept
  in the child's `Snapshot` and set by `ChildNode::say`, the one place a state changes. It is what
  `next_stop` would have returned last, and it says why a running child last rested.
- **`outstanding` is counts, not ids**, as `continuation.md` calls them "the roster's three
  counts", projected in one pass from every record not ended -- `Unread`, `Queued`, `Received` --
  and never from `summary`, which keeps a work child's one submission. The report and the events
  carry the ids.
- **Order.** The tree's: an agent before the agents it launched, in launch order, from each root
  (`owner`, `primary`) by id. A child whose parent's node is gone comes after the tree, in launch
  order, with what it launched below it: a root whose construction failed after its constructor
  launched a child loses its node, while the child, wedged as the failure releases it, stays
  resident until its kernel ends. Review found the first build, which walked only from
  parentless nodes, left such a child out of the roster while `children-max` still counted it;
  `a_failed_constructions_wedged_child_is_listed` is that case, and listed nothing before the
  fix. The alternative, keeping the failed root's node, would change what every reader of the
  registry sees of a launch that failed.
- **`seq`** is the registry's `State::seq` at the read: every change a `Status::seq` numbers at
  or below it is in the roster. Launches count; request changes do not, as `0003-48` defined it.
- **What a row is made of.** `ChildNode` keeps its `ChildKind`, its spawn's `name` and its
  `class`, which the registry did not keep before. The class is `module.qualname` from the
  instance the spawn line names; a root its owner constructs is the `ConstructSpec` class, `:` read
  as `.`, until its kernel declares the class, which then replaces it, so the row names what
  `agent.instance.started` names. A root's name is the one `Owner::construct` gives its spawn, the
  class's last segment.
- **A request heard of after the close is the report's.** The interpreter can make a request
  before it hears of the close that the host records after it; the interpreter's close ends it
  `closed`, but `at_close` had been taken, so the report never listed it while a roster taken
  between the two would list it outstanding. `State::record`, which `requested` now calls, adds
  such a request to `at_close`, so a roster taken after the close lists nothing the report does
  not. `CHANGELOG.md` says so.
- **An end told before its request is heard after it.** The interpreter registers a request in
  `_works` before it sends `agent.request`, so its close can send the request's `agent.ended`
  first; `Inner::ended` dropped an end for an id it did not know, and the request then stayed
  open, which the report, now listing it, called `terminated`. Once admission has closed,
  `State::ended_early` keeps such an end by id, `State::record` hands it back as the request is
  recorded, and `requested` hears it after the `agent.request.sent` it follows. An end kept for
  a request never told of stays until the session ends; before the close none is kept, since an
  end the interpreter sends then for a request it never announced -- one made of a child already
  released -- has no request coming to wait for. Review found it, as older than this task; the
  `at_close` change made it show.
- **No token usage.** The roster carries none; whether it should, from the ledger
  (`agent/ledger.rs`), or the owner keeps reading it from `model.round.*` events and the report,
  stays `rust-owner.md`'s open question.
- **No event, no close row.** A read publishes nothing and closes nothing; the stream stays the
  diagnostic surface, and `doc/reference/events.md` names the roster beside the stop read.
- **A primary session's roster is read between rounds.** `Session::roster(&self)` is the
  deliverable, and serves a `no_primary` session's owner at any time; reading one while a
  primary's round runs would need a reader cloned off the session first, as `Session::waits`
  is. Nothing asks for that yet.
- **`/simplify`** moved `requested`'s insertion into `State::record` (one lookup of the child,
  no clone), built each row once with the unlisted agents returned before anything is allocated,
  skipped ended records before the per-child map, and refreshed the class only for a root
  constructed in its kernel. The tests took up `Flag::awaited`/`blocked`, `child_runs`, a list
  form of `serving` (`serving_each`) and `stop_tests::ready_root_with`, which `ready_root` now
  calls. Not taken: storing the outgoing state as `last_stop` rather than cloning the incoming
  one, which saves a clone only for `Unavailable` and `Failed` and makes the read compute it; one
  watch instead of three for the wedged case, which the acceptance's "every read above" asks for;
  and reading `summary`'s and `quiet`'s `kind` from the node, filed as
  `plan/next/a-childs-kind-is-the-registrys.md`.
- **Tests.** `children.rs`'s unit tests project registries built by hand: the tree's order, the
  owner and a gone child left out, the three counts beside `summary`'s one held, `releasing`,
  `wedged`, the close and the interpreter's death, `last_stop` across a round, and a request
  heard of after the close. `agent/roster_tests.rs` runs a `Reviewer` root whose rounds launch
  children, each case that reads a child's state or holdings under the three watches of
  `stop_tests`. The retrying child waits `runner::FIRST_WAIT` of two seconds, the live first wait,
  so `next_attempt - since` is checked against two to sixteen seconds as stated. The wedged child
  blocks in `time.sleep` until a flag, as `children_tests`' wedge does: like `while True: pass`
  it never yields, and the flag lets the test end the kernel and see the row go. "Immediately
  after `agent.spawned`" reads the roster in the subscription's own loop, on each `agent.spawned`
  received. "The resident count" is checked against `Children::resident`, a test-only read of
  what `children-max` counts.
