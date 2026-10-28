# 0003-69 -- The docs describe what the phase added

## Context

`0003-38` writes the reader's account of the system `0003-01` through `0003-37` built: the Python
runtime, hosted objects, skills, typed agents, agent classes, host programs, and MCP demoted from
the way an agent acts to one integration surface among others. The planning round of 2026-10-28
appended `0003-39` through `0003-68` after it, from the requirements of the first application to
embed the harness (`plan/phase/0003-python/README.md`, "Tasks"), and kept `0003-38` where it was:
a thirty-task insertion is not the kind the five earlier renumberings were for, and the first
account should not wait on it. Each appended task documents its own addition as it lands --
its config keys in `doc/reference/config.md`, its events in `doc/reference/events.md`, its rows in
`lifecycle.md`'s close table, its `SECURITY.md` bullet where it grants something -- per
`next-task/SKILL.md`. What none of them owns is the account across them: what a Rust owner does
with a Session from construction to resume, what a facility is to the model and to the operator,
how a channel, a service and an MCP service differ to an agent that reaches all three, and what a
save is and is not.

Three pages `0003-38` writes or rewrites are changed again by the appended tasks and need a
second pass rather than a patch each: `doc/concepts/mcp-servers.md` and `doc/concepts/
mcp-trust-model.md`, which `0003-38` makes say MCP is one surface among others and `0003-54`
through `0003-57` then give the Session's own placements and a presenter; the Python-runtime
concepts page, whose account of agent classes `0003-41`, `0003-42` and `0003-45` change
(construction before attachment, facilities, the instance as `self`); and the host-programs
usage page, beside which a Rust embedder now needs its own.

Note that `doc/concepts/*.md` and `doc/reference/*.md` are symlinks into
`crates/outrig-cli/src/mcp_self/docs/`, so editing them edits shipped tool output, as `0003-38`
records.

## Goal

Someone reading the docs finds the system that exists after `0003-68`: what an embedder can do
from Rust, what an agent is given and sees, how messages, services and MCP services reach it,
what a save holds and a resume restores, and which key, event and command belongs to each.

## Deliverables

- **A usage page for Rust embedders**, `doc/usage/embedding.md`, beside the host-programs page:
  `SessionBuilder` with application code, facilities, services, channels and MCP declarations;
  constructing the primary and holding its handle (`0003-46`); the stop read, the per-round facts
  and the roster (`0003-48`, `0003-49`); the model-request limit and its reset (`0003-50`); save,
  sleep and resume with a storage adapter (`0003-58` through `0003-65`); and what the owner must
  never infer from the stream. `crates/outrig/README.md` gains a short example of the same, which
  compiles as a doctest since `lib.rs` includes the README.
- **The concepts pages brought to the appended system.** The Python-runtime page: construction
  before attachment, the facility dict and what a child is told of, the instance as `self`, the
  `task` alias, helpers against requests, contributions. `doc/concepts/mcp-servers.md` and
  `mcp-trust-model.md`: the Session's placements, the rule that a credentialed server is never
  primary-view, exact names, structured results, the three dispatch states, and that reconnect
  replays nothing. The hosted-objects page: a service beside a hosted object, with the same grant
  rule and the same events.
- **A concepts page for continuation**, `doc/concepts/continuation.md`: the pause against the
  close; what a save holds and never holds; designation and why nothing is captured by scan;
  the cut and its writers; save against sleep; the resume notice and what "the world may be ahead
  of the save" means; the format's version and its compatibility rule; and the CLI's `--resume`.
- **A concepts page for channels**, or a section of the runtime page (fork 1): the user channel
  as one external channel; plain against request kinds; wake policy; what accepted, refused and
  uncertain mean to the agent; that a round's end is not task completion and idle is the owner's
  signal.
- **The config reference for every key the appended tasks added**, in `doc/reference/config.md`:
  application code, facilities, services, external channels, MCP declarations and policy, the
  model-request limit, `selection` on an alias, the continuation keys, and the table of which
  settings apply to an owner-constructed root and which never do, extended from `0003-38`'s
  table for host programs.
- **`doc/reference/events.md` extended with every event type the appended tasks added**, checked
  against the emitting sites, with the producer record first (`0003-66`), the correlation rule
  (`0003-67`) and the audience each field belongs to (`0003-68`); and `doc/reference/cli.md` with
  `--resume` and any REPL command the continuation tasks add.
- **A `SECURITY.md` consistency check** against `plan/phase/0003-python/security.md` and the
  appended pages: what application code grants, that a facility dict is a view and not a grant,
  what a service grants, that a credentialed MCP server is never primary-view, and that a save
  holds message bodies and designated files and is the owner's to protect.
- Any `TODO: Incomplete` marker whose behavior is now real, dropped -- and any that is still
  honest, kept.

## Acceptance

- `python3 scripts/audit-doc-style.py` exits 0.
- **No page describes a mechanism as planned that `0003-68` has landed, and none as landed that a
  task left out.** Checked by reading each appended task's `## Decisions`.
- Every relative link resolves, the self-docs MCP server still serves the edited pages, and the
  symlink test passes.
- **Every config key the appended tasks added appears in `doc/reference/config.md`**, checked
  against `Config`'s fields rather than against memory.
- **Every event `type` the code emits appears in `doc/reference/events.md`**, checked against the
  emitting sites; the renderer's fixture sweep agrees.
- `cargo rustdoc -p outrig --lib -- -D warnings` passes and the README example compiles.
- `SECURITY.md` agrees with `security.md` and the appended pages on every point they changed,
  checked by reading.
- `cargo test --workspace` passes.

## Design forks

1. **One concepts page for channels, or a section of the Python-runtime page -- Open.** Start
   with a section, as `0003-38`'s fork 1 starts with one runtime page, and split only if it stops
   being readable.
2. **Whether the Rust embedding page and `crates/outrig/README.md` share one example -- Recommended:
   yes**, the README's being the shorter cut of the page's, so the doctest keeps the page honest.

## Dependencies

- **Hard: `0003-38`.** The first account, which this one extends rather than rewrites.
- **Hard: `0003-47`, `0003-50`, `0003-53`, `0003-57`, `0003-59`, `0003-65`, `0003-68`.** The last
  task of each cluster, so every subject documented here exists.
- **Soft: every task from `0003-39` to `0003-68`**, each of which drops its own markers. This one
  is done last.

## See also

- `plan/todo/0003-38-docs-describe-what-exists.md` -- the first pass, and the symlink note.
- `plan/phase/0003-python/README.md` -- the appended deliverables and the third milestone.
- `crates/outrig-cli/src/mcp_self/docs.rs` -- the `include_str!` sites and the symlink test.
