# 0003-16 -- The docs describe the system that now exists

## Context

`doc/` is design-first: `.claude/CLAUDE.md` says subsystem pages carry `TODO: Incomplete` until
their implementation lands. Individual tasks drop their own markers as behavior becomes real,
per `next-task/SKILL.md`. This task exists for the part that belongs to no single task.

`README.md` names the linked subsystems and says which matters most: "the MCP pages most of all,
since this phase demotes MCP from the way an agent acts to one integration surface among others."
`doc/concepts/mcp-trust-model.md` and `mcp-servers.md` currently describe MCP tool calls as *the*
way an agent acts. That is a cross-cutting claim, false the moment `run-new` exists, and owned by
no earlier task.

Two other pages have standing commitments. `doc/usage/sessions.md` carries the transcript-capture
TODO that `0003-13` retires. And `doc/reference/cli.md` and `doc/usage/run.md` describe a `run`
that is now one of three commands.

Note these pages are symlinks into `crates/outrig-cli/src/mcp_self/docs/`, so editing them edits
shipped tool output.

## Goal

Someone reading the docs finds the system that exists, including which command does what and where
MCP now sits.

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
- `cargo test --workspace` passes, since `mcp_self/docs.rs` `include_str!`s these files.

## Design forks

1. **One Python-runtime concepts page or several -- Open.** The design has fourteen pages; the
   docs should not. Start with one and split only if it stops being readable.
2. **Whether `doc/concepts/subagents.md` is touched here -- Recommended: no.** Subagents are
   unqueued, and the page describes a system that still works under `run`.

## Dependencies

- **Hard: 0003-13.** The sessions TODO cannot be dropped until events are written.
- **Soft: every earlier task**, each of which drops its own markers. This one lands last.

## See also

- `plan/phase/0003-python/README.md` -- the linked subsystems and why the MCP pages matter most.
- `crates/outrig-cli/src/mcp_self/docs.rs` -- the `include_str!` sites and the symlink test.
