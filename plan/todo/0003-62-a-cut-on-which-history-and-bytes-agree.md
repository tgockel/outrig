# 0003-62 -- A cut on which history and bytes agree

## Context

Three mechanisms order new work against the one-way close, and each has a hold the pause can
reuse. Rust's gate refuses a submission while `Table.closing` is set (`crates/outrig/src/python/
host.rs:971-987`, `submit`; `:494-500`, `Closing`; `:1763-1788`, `Table::close`), and a child's
runner opens no round once the session is closed (`crates/outrig/src/agent/runner.rs:74-76`). The
relay stops forwarding boundary requests when it closes, refuses what it holds and sends the
bindings process a control line behind the frames already forwarded
(`crates/outrig/src/python/hosted.rs:471-501`, `close`; `:592-662`, `first`); the bindings
process sets its flag when it reads that line (`crates/outrig/src/python/binding.py:292-304`,
`_control`; `:1738-1740`), and a live object's serving module takes the same line (`0003-36`).
The session's state moves `idle` to `closing` to `reported` with no way back
(`crates/outrig/src/harness/lifecycle.rs:1-10`, `:85-95`; `crates/outrig/src/harness/event.rs:
1022-1035`, `SessionState`). `shutdown` drains executions and forwarded calls until a deadline
(`crates/outrig/src/harness/session.rs:396-435`), and the bindings' stop lists what is left in the
process group (`crates/outrig/src/harness/report.rs:84-99`, `BindingStop::survivors`). The
workspace is a host directory (`crates/outrig/src/harness/builder.rs:723-771`, `same_paths`), so
its files are readable from the host without the interpreter. The builder takes owner-supplied
objects as traits -- `Secrets`, `Approver`, `SkillSource` (`builder.rs:187-191`, `:251-254`,
`:278-285`) -- and a host program reaches the engine over the owner protocol
(`crates/outrig/src/owner/mod.rs:12-28`). `0003-58` exports history at a quiescent boundary,
`0003-61` leaves a standing designation, `0003-53` gives an external channel's cut descriptors, and
`0003-54` places MCP services with the control `mcp-presenter.md` defines.

OR-SUS3 asks that one high-level request has OutRig "coordinate preparation, execution admission,
delivery mutation, interacting children and operations, selected-state capture, packaging and
storage" as internal phases; that "history and selected bytes must agree on the cut"; that
managed servers and background processes able to change selected state "require proven quiescence
or a sufficient explicit snapshot guarantee, even with no active RPC", since "a Python barrier
alone is insufficient"; that uncontrolled writers "require refusal or explicit degradation, not a
clean capture claim"; that "unrelated dirty files do not expand the selected set"; and that
"external operation uncertainty is recorded, not resolved by optimistic cancellation or replay".
`outrig-mcp-needs.md`, section 10.1 as corrected in 10.4, says the same of MCP writers for the
selected set.

**This task reverses a recorded decision, in part.** `lifecycle.md`, "Admission", says: "Once it
closes, a boundary request the owner has not forwarded to its binding is refused, a submission is
refused rather than run, and a child launch fails at once"; and the session's states run `idle`
to `closing` to `reported` with nothing returning from `closing` (`embedding.md`, "What the
embedder drives"). What this task adds beside them is a gate that holds new work while a cut is
established and reopens when the operation was a save or was abandoned. What it leaves as it is:
`close_admission()` (`session.rs:299-301`) stays one-way, the close stays the first step of a
shutdown, and "admission" keeps the design's meaning, the terminal close. The validation
(reversal 4, first part; the wording fix) found the gate additive -- "a session state between
`idle` and `closing` with a path back to `idle`, beside the one-way close", reusing the relay's
forwarding stop and the serving processes' flags -- and found that the requirements use
"admission" for both gates where the design's word means only the close (the conflict check's
tension 3). It also found that the agent's own tasks, `to_thread` workers and subprocesses are
writers the design lets outlive rounds, so a save-only cut needs the agent's cooperation in the
preparation round or a brief execution barrier (tension 6), and that a running hosted or
live-object call cannot be stopped (constraint 5), so a cut names it and refuses or degrades. The
adversarial review's issue 8 is the corollary: a facility the agent waits on returns promptly.
`continuation.md`, "The pause and the cut", designs what this task builds.

This task is two branches' worth. The first half is the pause: the hold and release in the gate,
the relay, the serving processes' control lines, the child runners and `Session::round`, with
the new states and their events. The second half is the drain, the writer quiescence, the barrier,
the capture, the manifest and the storage adapter. The seam is between the pause and the
drain-capture-package half: the first half is complete and testable on its own, with a save that
pauses and releases and captures nothing yet, and the maintainer may split the task there when it
is picked up, keeping this number for the first half.

## Goal

One owner call holds new work, drains and quiesces what can change the designated set, captures
history and designated bytes at one point, and hands an immutable package to a storage adapter, or
refuses or degrades naming the writer it could not control.

## Deliverables

- **The pause.** `Session::save(options)` (the spelling is this task's; `0003-63` finishes its
  outcomes and adds `sleep`) runs `0003-61`'s preparation round when the options ask for it, then
  moves the session into the pause -- `pausing` while running rounds end and the drain runs and
  `paused` while the cut is held, or one state (fork 1) -- published as every state is, with a path
  back to `idle`. The gate sits at the round boundary (fork 2): a round already running when the
  pause begins runs to its end with its submissions admitted, and no round opens while paused -- no
  child runner opens one, and `Session::round` answers that the session is paused. The pause is
  established when every running round has ended or the deadline passes; a round still running then
  makes the cut refuse naming it, or degrade under the options' policy. While paused the relay stops
  forwarding boundary requests and holds what arrives, within its bounds, forwarding bookkeeping as
  after the close; the bindings process and each live object's serving module are sent a control
  line that holds a request read after it rather than refusing it. A child launch and a request to a
  child queue as they do. Release forwards what was held, in order, sends the releasing control
  line, lets runners open rounds, and publishes `idle`. The pause is bounded by the operation's
  deadline.
- **`close_admission()` is unchanged**, and no path leads from `closing` back to `idle`; a close
  during a pause supersedes it, as the close rows below say.
- **The drain.** Running executions and forwarded hosted and MCP calls are given until the
  deadline. What still runs then is named -- an execution by `execid`, a hosted call by binding
  and member, an MCP call by service and tool, a live-object call by binding and member -- and the
  cut is refused, or degraded under the options' policy (fork 6).
- **Writers that can touch the designated set**, as `continuation.md` lists them: the bindings
  process, held, with every process left in its group listed and named as a possible writer; each
  MCP placement, paused or stopped through `mcp-presenter.md`'s control with its outcome
  reported, and one that cannot be controlled and can write under a designated path's mount named
  as uncontrolled; a live object's call running at the deadline named; and the agent's own tasks,
  threads and subprocesses, which the preparation round told it to finish, checked by a **brief
  execution barrier**: with no round open and none opening, each designated path's size and
  digest is read from the host mount, the barrier's interval passes (fork 3), and each is read
  again; a difference names an uncontrolled writer by path, and the loop's tasks and the
  interpreter's known subprocesses are listed beside it. No difference with a writer listed is
  `degraded` naming the writers, since nothing proved they were done, and refused under the clean
  policy; no difference and nothing listed is clean. A writer that cannot reach any designated
  path imposes nothing, and no undesignated file is read.
- **Capture and packaging.** After the barrier: `0003-58`'s export of every agent's store; each
  designated file read from the host mount into a staging directory OutRig owns (fork 4); each
  designated datum encoded; the manifest written -- the format version, the checkpoint identity
  `0003-63` finishes, the stream sequence of the cut, the designated set with each entry's digest
  and host path, the entries refused or unsupported, the writers quiesced and the writers named
  as uncontrolled, the restore obligations (`application-code.md`'s identity, each facility,
  channel and MCP service by name), and empty disposition and uncertainty lists for `0003-65` to
  fill. The package is then handed to the storage adapter as staged and is immutable from that
  point; a save releases the pause and returns.
- **Open work refuses the cut**, until `0003-65`: a request open on any child, a child that is not
  idle, or an external request owed a reply makes the cut refuse naming it. A save with no
  designation and no open work is a history-only save (fork 7).
- **The storage adapter's interface** (fork 5): a trait on the builder with a method that takes
  a staged package under its identity and a method that reads one back by identity; `0003-63`
  adds publication and acknowledgement. For a host program, a Python object passed to
  `outrig.Session(...)` whose methods the engine calls over the owner connection as requests the
  program answers, JSON crossing both ways and the package a Python object in the program. OutRig
  builds the test adapter its acceptance needs and no other; the CLI's is `0003-63`'s.
- **Cancellation.** An owner that abandons the operation before staging releases the pause and
  gets an outcome that lists the preparation's effects -- the round's turns, the files the agent
  wrote, the designation it made -- and claims no rollback.
- **Events**: `session.state` gains the pause; `continuation.cut.staged` with the identity, the
  designated set and the writers listed; `continuation.cut.refused` and `continuation.cut.degraded`
  naming the blocker, or the writers listed and unproven; the adapter's calls as integration audit,
  with the identity and the outcome. Each added to `doc/reference/events.md` with its category.
- **`lifecycle.md`'s close table** gains rows: a pause in progress at the close is superseded --
  held submissions and requests are refused with the closing reason as the close refuses them, the
  releasing control line is not sent and the closing one is; a staged package at the close is
  kept, since the close discards nothing; a barrier or a capture in progress at the close is
  abandoned and the operation reports `failed`, the close as the reason.
- **`## Decisions` records the reversal**: the sentences quoted above and what was added beside
  each; that `close_admission()` is untouched; the state names; that the gate sits at the round
  boundary, and how each of the three mechanisms holds and releases; the barrier's interval; and the
  adapter's shape.
- `crates/outrig/public-api.txt` regenerated.

## Acceptance

Each is a mock-driven test unless it says otherwise.

- **The pause gates at the round boundary, and releases in order.** A save begins while a mock
  model's round runs: the round's next submission runs and its result reaches the model, the round
  ends, and the pause is established then; no round opens until the release, and one with a changed
  summary opens after it; a round that outlasts the deadline makes the save refuse naming it. A
  boundary request a loop task makes during the pause is forwarded after it, in the order made; the
  bindings fixture reads the holding line, holds a request read after it, and runs it after the
  releasing line; `round()` answers paused during and drives a round after; `session.state` reads
  `idle`, the pause's states, `idle`.
- **`close_admission()` during a pause** closes: held submissions and requests are refused with
  the closing reason, the session moves to `closing` and never back, and the operation reports
  `failed` naming the close.
- **The drain.** An execution that ends within the deadline is in the exported history; one that
  does not makes a save refuse naming its `execid`; a hosted call on the slow fixture binding
  still running at the deadline refuses naming the binding and member, or degrades under the
  policy with the call listed; `0003-36`'s live-object fixture does the same.
- **History and bytes agree.** The agent writes `index.json` in an execution, designates it, and
  the round ends; the save's exported history's last turn holds the write, and the captured bytes
  are the written bytes, with the digest in the manifest.
- **Unrelated dirty files are not captured.** A second file the agent wrote and did not
  designate is absent from the package and from the manifest.
- **An unproven writer degrades.** A loop task alive at the barrier with no difference in the
  designated set: the cut is `degraded` naming the task under the degraded policy and refused naming
  it under the clean one; with no task and no subprocess listed, the cut is clean.
- **A sidecar writer.** A test MCP placement (`0003-54`'s fixture) appending to a designated file
  with no call in flight: with the placement controllable, it is paused, the barrier passes, the
  package holds the bytes as of the pause, and the writer resumes after staging; with the
  placement marked uncontrollable, the cut refuses naming the service, or degrades under the
  policy with the service listed. A subprocess the agent started that keeps writing a designated
  file is found by the barrier: the cut refuses naming the path and lists the subprocess.
- **The bindings group.** With a fixture library that leaves a child process running, the cut
  lists the process beside the bindings process and degrades or refuses per policy.
- **Immutable once staged.** The test adapter's attempt to write into the staged package fails,
  and the manifest's digest matches the package after the session has run on.
- **Open work refuses.** A child holding a taken request, a running child, and an external
  request owed a reply (`0003-52`'s fixture) each make the cut refuse naming them.
- **Cancellation.** A save abandoned during the preparation round reopens, and its outcome lists
  the round's turns, the file the agent wrote to prepare and the designation it made; nothing is
  undone.
- **The adapters.** The test adapter receives a staged package under an identity and reads it
  back equal; a host program's Python adapter receives the same calls over the owner connection
  in a mock-model test, and the program-side object sees Python values.
- **Gated e2e, a real container, no model**: a session whose primary writes a file, designates it
  and is saved has the file's bytes in the package, read from the host mount.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The pause's states -- Recommended: two, `pausing` for the drain and `paused` for the held
   cut**, so a monitor can tell a long drain from a held barrier. The alternative is one state.
2. **Where the gate sits -- Recommended: the round boundary.** A round running when the pause begins
   runs to its end and no round opens after it, so no submission is ever held or refused during a
   save: a held submission would hold the round open while the export refuses on a running round and
   the barrier waits for none to be open, and a refusal would reach the model as a result it acts
   on, teaching it something false in a session that continues. The alternative gates each
   submission, held until the release or refused with a reason, which is the close's shape and the
   deadlock's.
3. **The barrier's interval -- Open; recommended: a few hundred milliseconds, under a
   `[continuation]` key with a ceiling.** Longer catches slower writers and holds the session
   longer; the measurement that decides it is the writer the test suite can build.
4. **Where staging lives -- Recommended: a directory beside the session's log directory, owned
   by OutRig, handed to the adapter by path.** Bytes in memory are the alternative, and a
   designated directory can be large.
5. **The adapter's shape -- Recommended: a trait with `stage(identity, package)` and
   `read(identity)`, which `0003-63` extends with `publish` and `acknowledged`.** The alternative,
   one method that both stages and publishes, cannot tell staged from committed.
6. **Refuse or degrade by default -- Recommended: refuse, with `allow_degraded` in the options.**
   A clean claim is never made either way; refusal makes the owner choose degradation knowingly.
7. **A history-only save -- Recommended: allowed**, since a conversation is worth saving on its
   own and `0003-58`'s export is exactly that; the alternative requires a designation.

## Dependencies

- **Hard: `0003-53`, `0003-54`.** An external channel's cut descriptors; MCP placements and the
  control that pauses or stops one.
- **Hard: `0003-58`, `0003-61`.** The export the cut calls, and the designation it captures.
- **Hard: `0003-19`, `0003-22`.** The session API and its states; the relay.
- **Hard: `0003-29`.** The owner connection the host program's adapter is called over.
- **Soft: `0003-36`.** The live object's serving module takes the control lines where `0003-36` has
  landed; the bindings process's control line needs nothing of it, so the Rust path does not wait on
  it.
- **Hard: `0003-30`.** The children's registry the open-work refusal reads.
- **Soft: `0003-56`.** An MCP call's dispatch states, which name a call still in flight.
- **Soft: `0003-51`, `0003-52`.** External channels whose requests refuse the cut until the
  dispositions task that follows lands.

## See also

- `plan/phase/0003-python/continuation.md` -- "The pause and the cut", "What a package
  holds, and what it never holds", "Constraints carried from the validation".
- `plan/phase/0003-python/lifecycle.md` -- "Admission", "Closing a session", "The close table".
- `plan/phase/0003-python/execution-and-rounds.md` -- "No rollback, ever", "One at a time".
- `plan/phase/0003-python/hosted-objects.md` -- "Live objects", "Lifetime".
- `plan/next/cancel-a-running-hosted-call.md` -- why a running call is named, not stopped.
- `crates/outrig/src/python/host.rs`, `hosted.rs`, `binding.py`; `crates/outrig/src/harness/
  lifecycle.rs`, `session.rs`, `builder.rs`, `report.rs`; `crates/outrig/src/agent/runner.rs`.
