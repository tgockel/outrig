# 0003-30 -- The docs describe the system that now exists

## Context

This task was `0003-16` until the planning round of 2026-09-30 renumbered it to come after the
tasks it documents, and `0003-29` until the round of 2026-10-01 added
`0003-29-an-agent-class-answers-requests` before it. The finished tasks `0003-01` and `0003-02`
refer to it by its first number, as the task that documents what `run-new` does, and the queue as
committed before 2026-10-01 refers to it by the second; in the current queue, `0003-29` is the
agent-class task.

`doc/` is design-first: `.claude/CLAUDE.md` says subsystem pages carry `TODO: Incomplete` until
their implementation exists. Individual tasks drop their own markers as behavior becomes real,
per `next-task/SKILL.md`. This task exists for the part that belongs to no single task.

`README.md` names the linked subsystems and says which matters most: "the MCP pages most of all,
since this phase demotes MCP from the way an agent acts to one integration surface among others."
`doc/concepts/mcp-trust-model.md` and `mcp-servers.md` currently describe MCP tool calls as *the*
way an agent acts. That is a cross-cutting claim, false the moment `run-new` exists, and owned by
no earlier task.

Two other pages have standing commitments. `doc/usage/sessions.md`'s transcript-capture TODO is
already retired: `0003-13` wrote the event log and `doc/reference/events.md`, which the event types
later tasks add must extend. And `doc/reference/cli.md` and `doc/usage/run.md` describe a `run`
that is now one of three commands.

Note these pages are symlinks into `crates/outrig-cli/src/mcp_self/docs/`, so editing them edits
shipped tool output.

The phase also grew after this task was first written. Hosted objects and boundary policy, the
embedding API, skills and typed agents all ship in it (`0003-16` through `0003-29`), and each
implementing task documents its own behavior when it is done. What none of them owns is a
reader's account across them: the config reference as a whole, the REPL's new commands in one
place, a concepts page a person reads before declaring a binding, and `SECURITY.md` saying the same
thing as the code.

## Goal

Someone reading the docs finds the system that exists, including which command does what, where
MCP now sits, and what a binding lets an agent do on the host.

## Deliverables

- `doc/concepts/mcp-trust-model.md` and `doc/concepts/mcp-servers.md` rewritten so MCP is one
  integration surface rather than the way an agent acts. The trust-model invariant is unchanged
  and should stay legible: an agent cannot grow its own environment.
- A concepts page for the Python runtime -- what an execution is, what a round is, that names
  persist, and that waiting watches channels. New page rather than an addition, since it is the
  phase's subject.
- `doc/reference/cli.md` and `doc/usage/run.md`: `run-new`, `run-legacy`, and what `run` still
  means, with the retarget named as later work so nobody plans around it.
- **Vocabulary, once.** A round contains turns; the interpreter is the process and a kernel is one
  agent's environment. The docs currently use "turn" in the 0.2.x sense throughout, which is
  correct for `run` and wrong for `run-new`, so the pages must say which system they describe.
- **The config reference for everything the phase added**, in `doc/reference/config.md`:
  `[bindings]` with its tagged path form, the approval a repo-declared binding needs, the order
  bindings' packages take on `sys.path`, the names a binding cannot take, and `serialize` under
  `[bindings.<name>]`, which gives a binding whose library is not thread-safe one call at a time
  where the default serves each connection on its own thread; `[policy]`, `[[policy.rules]]` and
  `[policy.evaluator]`, with what a repo config may and may not set there, and that the policy and
  the evaluator's model are read from the operator's layer -- the global config, or the `Config`
  an embedder passes; `[events]`, with its precedence rule: a `mode` the global config sets stands,
  and a repo's applies only when the global config says nothing (`0003-22`); `subagent-width-max`
  as `run`'s key, which the new loop does not read; `subagent-depth-max`, which applies to both
  loops; `children-max` (default 64), which counts resident children, wedged ones included until
  they are reclaimed, and `model-concurrency-max` (default 8), which bounds model requests in
  flight -- the session-wide limits `0003-25` adds, neither of which bounds CPU or memory inside an
  admitted kernel; and the per-tree token budget.
- **`doc/reference/events.md` extended with every event type the phase added after `0003-13`**:
  the session states and the in-memory stream's gap reporting (`0003-19`), a hosted request's
  receipt, decision, dispatch and outcome (`0003-21`, `0003-22`), the evaluator's verdicts and
  usage (`0003-23`), `agent.request.*` (`0003-25`), `agent.call.*` (`0003-26`),
  `agent.instance.*` (`0003-29`), and `skill.directive.received` and `skill.invocation.*`
  (`0003-28`), each with its category.
- **`doc/reference/cli.md` lists `/approve <id>`, `/deny <id>` and `/name text`** among
  `run-new`'s commands, with what each does, that a pending request waits until it is answered,
  interrupted or the session closes, and that built-in commands win over a skill's name. For
  `/name text`: the REPL parses nothing; the line is delivered to the agent, whose
  `outrig.skills.invoke(name, text)` has a typed agent call parse the text into the skill's
  `<Skill>Params`, and an agent that understood the text itself calls `invoke(name, params=...)`,
  which makes no model call. Either way the instance is on the invocation's event.
- **A concepts page on hosted objects and the boundary**: what a binding is, that a hosted object
  acts with the host user's authority, the same paths on both sides, what crosses and what is
  refused, the default of allow with every request published as events, rules and escalation,
  and the evaluator.
- **Skills and typed agents in the Python-runtime concepts page**: the two skill roots and the
  shadowing rule, `/name`, `outrig.skills` -- `invoke(name, text)` deriving `<Skill>Params` from
  the entry's signature and having a typed agent call parse the text into it, and
  `invoke(name, params=...)` making no model call -- `runtime.spawn`, `@outrig.agent` and
  `outrig.Agent`, request channels and replies, completion and repair, releasing a child, how a
  child's spend is added to its call, the skill invocation and the round, and that a child has no
  user channel.
- **The embedding API documented**: `crates/outrig/README.md` shows a session built, driven and
  shut down, and every public item of the API has rustdoc.
- **A `SECURITY.md` consistency check**: its in-scope and known-boundaries lists against what
  shipped -- a repo `[policy]` that loosens a global one is in scope, as a repo `[network]` that
  widens the egress policy is; a hosted object's host authority and the allow-and-publish
  default are known boundaries.
- Any `TODO: Incomplete` marker whose behavior is now real, dropped -- and any that is still
  honest, kept.

## Acceptance

- `python3 scripts/audit-doc-style.py` exits 0. This is the CI gate and `doc/` is what it covers.
- **No page describes MCP tool calls as the way an agent acts.** Checked by reading, since a grep
  for the phrasing will not find every form of the claim.
- Every relative link resolves -- the audit's link check covers this, and the symlinked pages make
  it easy to get wrong.
- The self-docs MCP server still serves the edited pages; the existing test that catches a symlink
  being replaced by a real file still passes.
- A reader can tell from `cli.md` alone which of the three commands to type.
- **Every config key the phase added appears in `doc/reference/config.md`**, checked against
  `Config`'s fields rather than against memory.
- **Every event `type` the code emits appears in `doc/reference/events.md`**, checked against the
  emitting sites rather than against memory.
- `cargo rustdoc -p outrig --lib -- -D warnings`, the strict rustdoc check CI runs, passes, and
  `crates/outrig/README.md`'s example compiles as a doctest -- `lib.rs` includes the README as the
  crate's front page, so its example is one.
- `SECURITY.md` agrees with `plan/phase/0003-python/security.md` on every point the phase changed,
  checked by reading.
- `cargo test --workspace` passes, since `mcp_self/docs.rs` `include_str!`s these files.

## Design forks

1. **One Python-runtime concepts page or several -- Open.** The design has well over a dozen
   pages; the docs should not. Start with one, beside the hosted-objects page, and split only if it
   stops being readable.
2. **Whether `doc/concepts/subagents.md` is touched here -- Recommended: yes, with a pointer.**
   The page describes `run`'s subagents, a system that still works under `run`, so it stays. It
   gains a note that `run-new`'s children (`0003-25`) are a different system, described in the
   Python-runtime concepts page, and that the width cap it states is `run`'s alone: `run-new`'s
   children are not under it, and are bounded by `children-max` and `model-concurrency-max`.

## Dependencies

- **Hard: `0003-13`.** It wrote `doc/reference/events.md`, which this task extends with every
  event type the later tasks add.
- **Hard: `0003-22`.** `/approve`, `/deny` and `[policy]` must exist before the reference
  documents them.
- **Hard: `0003-23`.** `[policy.evaluator]` must exist before the reference documents it.
- **Hard: `0003-26`.** The typed-agent half of the concepts page describes `@outrig.agent`.
- **Hard: `0003-28`.** `/name` and the skills half of the concepts page need the directive.
- **Hard: `0003-29`.** The concepts page describes `outrig.Agent`, request channels and release,
  and the events reference gains the `agent.instance.*` family.
- **Soft: every earlier task**, each of which drops its own markers. This one is done last.

## See also

- `plan/phase/0003-python/README.md` -- the linked subsystems and why the MCP pages matter most.
- `crates/outrig-cli/src/mcp_self/docs.rs` -- the `include_str!` sites and the symlink test.
