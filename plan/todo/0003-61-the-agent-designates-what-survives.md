# 0003-61 -- The agent designates what survives

## Context

Nothing in the runtime lets an agent say what of its own state must survive a session, and nothing
captures any of it. The workspace is a host directory mounted into the container: with bindings,
the container path becomes the host path so a path names one file on both sides
(`crates/outrig/src/harness/builder.rs:723-771`, `same_paths`), and otherwise `spec.workspace.host`
is where the container's `/workspace` is. A host-opened round with a fixed opening exists --
`Agent::opened` (`crates/outrig/src/agent/runner.rs:166-170`), the orienting round's `ORIENTING`
(`crates/outrig/src/agent/orientation.rs:358-362`), `0003-37`'s warning round -- and `0003-60`
makes such a round exempt from `0003-37`'s count. `runtime` is the object agent code acts through
(`crates/outrig/src/python/interpreter.py:3840`, `Runtime`), and the serializable subset is what
a channel contract may carry, checked by `outrig.schema` (`messages.md`, "Contracts").

OR-SUS2 asks that, for a sleep, the model is told the interpreter will restart and given a
bounded opportunity "to identify/designate and prepare important files or supported state,
possibly none"; that for a save-only checkpoint preparation "concerns possible recovery from that
save, while the current interpreter continues", without "falsely announc[ing] an imminent
restart"; that OutRig "never infer[s] unselected files or automatically capture[s] dirty/untracked
files or the whole workspace from scans/Git status"; that the notice and the preparation are
preserved in history; that omitted or unsupported selections are reported; and that "facility
associations, channels and application-code identity are not files the LLM must rediscover". Its
open choices are the designation API and whether periodic saves reuse or refresh the selection.
The planning's authoritative correction (`decisions.md`, section 8;
`preservation-correction-summary.md`) puts the choice with the agent and leaves the API open.

The validation found that the designate-and-prepare notice is a host-opened round like `0003-37`'s
warning round, which must be exempt from its count (the conflict check's tension 2), and that a
save-only cut needs the agent's cooperation in this round or a brief execution barrier for the
designated set, since the agent's own tasks, `to_thread` workers and subprocesses outlive rounds
(tension 6). `continuation.md`, "Designation", designs what this task builds; `0003-62` consumes
the standing designation at the cut.

## Goal

Before a sleep or a save, the agent is told which it is and designates the files and data that
must survive, or none, through a runtime API whose result is recorded, and nothing is captured
that it did not designate.

## Deliverables

- **The runtime API** (the spelling is fork 1), reachable from any execution of any agent: a call
  that designates paths and data, one that withdraws, and one that reads back the standing
  designation. A path is a container path under a mounted directory -- the workspace or a mount
  -- naming a regular file or a directory, recorded with the host path the cut will read; a datum
  is a value in the serializable subset under a name, checked by `outrig.schema` as a channel
  contract is. A path outside every mount, a path that is neither a file nor a directory under
  one, a datum outside the subset, and a reference to a live object, a hosted proxy or a facility
  are refused at the call with the reason, and nothing is recorded for them. The designation is
  the session's, each entry attributed to the agent that made it, and stands until withdrawn or
  the session ends; `help()` on the namespace says what it is for and what it refuses.
- **The preparation round**, opened by the host on the primary, or on each root agent of a
  session with none, as `0003-60`'s consultation round is, exempt from `0003-37`'s count and
  moving no baseline. Its fixed opening states which operation this is; what is designated so far,
  by path and name; that OutRig carries the facilities, channels, services and application code
  without designation; that an undesignated file is not promised to survive and that nothing is
  inferred from the workspace; and that the agent's own tasks, threads and subprocesses that write
  to what it designates must finish or stop before the round ends. The sleep opening says the
  interpreter will restart and that every name in the namespace will be gone; the save opening
  says the save is a point a later session may continue from while this interpreter runs on, and
  never that a restart is imminent. Drafts, for the task to tune:

  > [outrig] this session will be put to sleep: the interpreter will restart in a new session,
  > every variable you hold will be gone, and your conversation will be restored. Designate the
  > files and data that must survive -- or none -- with `runtime.continuation.designate(...)`;
  > so far you have designated nothing. Prepare them now: write what is in memory that matters
  > to a file and designate it, and finish or stop any task, thread or subprocess still writing
  > to what you designate. Files you do not designate are not promised to survive; nothing is
  > inferred from the workspace. Your bindings, channels, services and application code come
  > back without designation.

  > [outrig] a save is being taken: a later session may continue from it, and this interpreter
  > runs on afterwards. Check what you have designated for recovery -- so far `index.json` and
  > the datum `ticket` -- and change it with `runtime.continuation.designate(...)` or
  > `.withdraw(...)` if what matters has changed; finish or stop any task still writing to what
  > you designate. Nothing restarts now.

- **Recording.** The round, the designation calls it makes and whatever the agent ran to prepare
  are turns of history as any round's are, so the resumed model reads its own preparation. When
  the round ends, the host reads the standing designation and emits `continuation.designated`
  with each path and host path, each datum's name and type, each entry's agent, and every refusal
  the round's executions met; `0003-62`'s manifest lists what was designated, what was captured,
  and what was refused or unsupported.
- **Reuse or refresh** for a periodic save, per fork 2, exposed as an option on the operation
  `0003-62` builds.
- **The orientation** gains one sentence in `rest()` (`crates/outrig/src/agent/orientation.rs:
  111`) naming the API and that it exists for saves and sleeps, costed as every preamble sentence
  is; nothing more, since the round carries the instruction when it matters.
- **Events**: `continuation.designated`, execution diagnostics, added to `doc/reference/events.md`.
- **`lifecycle.md`'s close table** gains a row: a preparation round running at the close runs on
  as any round does; the standing designation ends with the session, since no cut was taken.
- **`## Decisions` records** the API's spelling, the reuse-or-refresh choice, what "supported
  data" means, and which agents the round reaches.
- `crates/outrig/public-api.txt` regenerated, for the event.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **The maintainer's example.** The mock agent writes `index.json` under the workspace and
  designates it with a datum `ticket = {"upload": "u-42"}`; the read-back lists both with the
  file's host path; a second dirty file the agent wrote and did not designate is not in the
  designation; `continuation.designated` names the two entries and the agent that made them.
- **Explicitly none.** A round in which the agent designates nothing ends with an empty
  designation and an event saying so.
- **Refusals.** A path outside every mount, a path to a socket, a datum holding a set, and a
  proxy to a fixture binding are each refused at the call with a message naming the reason, and
  none is recorded; the round's result carries the refusal, and the event counts it.
- **Two openings.** The sleep round's opening says the interpreter will restart and names the
  standing designation; the save round's opening says the interpreter runs on and never says
  "restart"; both name the API and the undesignated-files rule. Asserted on the text.
- **The round is exempt.** A request child that took a request and would be warned next is
  given a preparation round: it opens, ends, and is followed by the warning and the cancel as
  without it; no attempt is counted.
- **Recorded in history.** The exported history (`0003-58`) of a session that ran a sleep round
  holds the opening as the round's prompt and the designation call in a turn's source.
- **Reuse or refresh**, as fork 2 decides, with a test per branch of the option.
- **Attribution.** A child that designates a datum has the entry attributed to its agent id, and
  the primary's read-back lists it as the session's.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The API's namespace and spelling -- Open; recommended: `runtime.continuation` with
   `designate(paths=(), data={})`, `withdraw(paths=(), data=())` and `designated()`.** A namespace
   keeps the three names together under one `help()` and leaves `runtime`'s top level as it is;
   the alternative, `runtime.preserve(...)`, reads as a promise the runtime does not make.
2. **Periodic saves reuse or refresh -- Open; recommended: the standing designation persists,
   and a save-only operation opens the round when the owner asks for it or when no designation
   exists.** Every save then costs no model call by default and the agent's last word stands; the
   alternative opens the round on every save, which spends a call per checkpoint for an answer
   that rarely changes. A sleep always opens the round.
3. **Supported data -- Recommended: the serializable subset, checked by `outrig.schema`**, since
   it is the one shape every side of this system already decodes. The alternative, opaque JSON
   text, moves the check to the agent and loses the type.
4. **Whether a local child may designate -- Recommended: yes, attributed**, since a child that
   built an index should be able to say so without its parent's help. The alternative confines
   the API to root agents and asks parents to relay.
5. **Which agents the round reaches -- Recommended: the primary, or each root agent of a session
   with none**, as `0003-60`'s fork 5 has it; a child is ended or refuses the cut (`0003-65`).

## Dependencies

- **Hard: `0003-39`.** The API is interpreter code.
- **Hard: `0003-60`.** The host-opened round with a fixed opening and its exemption from
  `0003-37`'s count.
- **Hard: `0003-26`.** `outrig.schema`, which checks a datum.
- **Hard: `0003-34`, `0003-37`.** Agent classes, and the rule the round is exempt from.
- **Soft: `0003-35`, `0003-46`.** Root agents the round reaches.
- **Soft: `0003-42`.** Facility references, which the opening says come back without designation.

## See also

- `plan/phase/0003-python/continuation.md` -- "Designation", "What a package holds, and
  what it never holds".
- `plan/phase/0003-python/messages.md` -- "Contracts", the serializable subset.
- `plan/phase/0003-python/agent-classes.md` -- "The child's rounds".
- `plan/todo/0003-37-a-request-is-worked-on-or-cancelled.md` -- the warning round's shape.
- `crates/outrig/src/agent/orientation.rs`, `runner.rs`; `crates/outrig/src/harness/builder.rs`
  (`same_paths`); `crates/outrig/src/python/interpreter.py` (`Runtime`).
