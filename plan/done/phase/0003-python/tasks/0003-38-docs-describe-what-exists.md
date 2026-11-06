# 0003-38 -- The docs describe the system that now exists

## Context

This task was `0003-16` until the planning round of 2026-09-30 renumbered it to come after the
tasks it documents, and `0003-29` until the round of 2026-10-01 added
`0003-29-an-agent-class-answers-requests` before it. The finished tasks `0003-01` and `0003-02`
refer to it by its first number, as the task that documents what `run-new` does, and the queue as
committed before 2026-10-01 refers to it by the second; in the current queue, `0003-34` is the
agent-class task. A third renumbering, in the planning round of 2026-10-09, inserted the facade
task, `0003-21`, before the relay task, `0003-22`, and shifted the tasks then numbered `0003-21`
through `0003-30` by one, which made this task `0003-31`. A fourth, in the planning round of
2026-10-15 that folded in the host-program requirements, inserted six tasks between `0003-22`
and this one and moved `0003-23` through `0003-31` again, which made this task `0003-37`.
A fifth, in the planning round of 2026-10-26 that promoted `potential/one-shot-nudging.md` to
`0003-37`, inserted that task before this one, which made this task `0003-38`.

The planning round of 2026-10-28 appended `0003-39` through `0003-70` after this task rather than
renumbering it a sixth time: thirty tasks from the first embedding application's requirements,
each documenting its own addition, and `0003-70` as the cross-cutting documentation pass for what
they add. This task keeps its scope, the system `0003-01` through `0003-37` built; where a later
task changes a page this one writes, that task edits the page.

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

The phase also grew after this task was first written. Hosted objects, the embedding API, skills and
typed agents all ship in it (`0003-16` through `0003-34`), and each implementing task documents its
own behavior when it is done. What none of them owns is a reader's account across them: the config
reference as a whole, the REPL's new commands in one place, a concepts page a person reads before
declaring a binding, and `SECURITY.md` saying the same thing as the code.

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
- `doc/reference/cli.md` and `doc/usage/run.md`: `run-new`, `run-legacy`, what `run` still
  means, and `run-new <script.py>` as the host-program form (`0003-27`), with the retarget named as
  later work so nobody plans around it.
- **A usage page for host programs**, `doc/usage/host-programs.md`: `outrig run-new <script.py>`,
  the program's argv, cwd and streams, `async with outrig.Session(...)`, `session.inject`,
  `session.events()`, `session.shutdown_report`, `outrig.ShutdownException`, `outrig.config()`,
  `outrig.Agent` declared and driven from the program, and what the engine does when the program
  exits, dies or ignores a signal (`0003-27`, `0003-29`, `0003-35`, `0003-36`); and the Python owner
  API's reference beside the Rust one, every public name with its `help()` text.
- **Vocabulary, once.** A round contains turns; the interpreter is the process and a kernel is one
  agent's environment. The docs currently use "turn" in the 0.2.x sense throughout, which is
  correct for `run` and wrong for `run-new`, so the pages must say which system they describe.
- **The config reference for everything the phase added**, in `doc/reference/config.md`:
  `[bindings]` with its tagged path form, the approval a repo-declared binding needs, the order
  bindings' packages take on `sys.path`, the names a binding cannot take, and `serialize` under
  `[bindings.<name>]`, which gives a binding whose library is not thread-safe one call at a time
  where the default serves each connection on its own thread; `[events]`, with its precedence rule:
  a `mode` the global config sets stands, and a repo's applies only when the global config says
  nothing (`0003-24`); `subagent-width-max` as `run`'s key, which the new loop does not read;
  `subagent-depth-max`, which applies to both loops; `children-max` (default 64), which counts
  resident children, wedged ones included until they are reclaimed, and `model-concurrency-max`
  (default 8), which bounds model requests in flight -- the session-wide limits `0003-30` adds,
  neither of which bounds CPU or memory inside an admitted kernel; the per-tree token budget; the
  `[host]` table with the cooperative grace a host program gets before the engine escalates
  (`0003-29`); and a table of which settings apply to a host program's session and which do not
  (R7): `default-image`, `[workspace]`, `[models]`, `[providers]`, `default-model` and `[events]`
  apply; a primary `preamble`, `[agents.<name>]` and `default-agent` do not, and never select a
  host-declared class's model, image or instructions.
- **`doc/reference/events.md` extended with every event type the phase added after `0003-13`**: the
  session states and the in-memory stream's gap reporting (`0003-19`), a hosted request's receipt,
  dispatch and outcome (`0003-22`), `agent.request.*` (`0003-30`, and the host's cancel, which
  `0003-37` records with its cause), `agent.call.*` (`0003-31`), `agent.instance.*` (`0003-34`),
  `skill.directive.received` and `skill.invocation.*` (`0003-33`), and `binding.injected`
  (`0003-36`), each with its category.
- **`doc/reference/cli.md` lists `/name text`** among `run-new`'s commands, with what it does, and
  that built-in commands win over a skill's name. The REPL parses nothing; the line is delivered to
  the agent, whose `outrig.skills.invoke(name, text)` has a typed agent call parse the text into the
  skill's `<Skill>Params`, and an agent that understood the text itself calls
  `invoke(name, params=...)`, which makes no model call. Either way the instance is on the
  invocation's event.
- **A concepts page on hosted objects and the boundary**: what a binding is, that a hosted object
  acts with the host user's authority, the same paths on both sides, that a hosted object is used
  as its library documents and a call on it blocks the agent's loop until the host answers while
  `asyncio.to_thread` keeps the loop turning, `outrig.bindings[name]` as the authoritative lookup
  beside the convenience global, a live object a host program injects beside a factory binding,
  what crosses and what is refused, and that the binding is the grant: every request to a bound
  object runs and is published as events.
- **Skills and typed agents in the Python-runtime concepts page**: the two skill roots and the
  shadowing rule, `/name`, `outrig.skills` -- `invoke(name, text)` deriving `<Skill>Params` from
  the entry's signature and having a typed agent call parse the text into it, and
  `invoke(name, params=...)` making no model call -- `runtime.spawn`, `@outrig.agent` and
  `outrig.Agent`, request channels and replies, completion and repair, the two nudges and the
  cancel that ends a request its child does not answer (`0003-37`), releasing a child, how a
  child's spend is added to its call, the skill invocation and the round, and that a child has no
  user channel.
- **The embedding API documented**: `crates/outrig/README.md` shows a session built, driven and
  shut down, and one started with no primary agent (`0003-28`); every public item of the API has
  rustdoc.
- **A `SECURITY.md` consistency check**: its in-scope and known-boundaries lists against what
  shipped -- a repo-declared binding that starts without the operator's approval is in scope, as a
  repo `[network]` that widens the egress policy is; a hosted object's host authority, and the
  binding as the grant with every request published, are known boundaries; a host program is trusted
  host code with the user's authority, not a sandbox, and a live object it injects acts with that
  authority (`0003-27`, `0003-36`).
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
   gains a note that `run-new`'s children (`0003-30`) are a different system, described in the
   Python-runtime concepts page, and that the width cap it states is `run`'s alone: `run-new`'s
   children are not under it, and are bounded by `children-max` and `model-concurrency-max`.

## Dependencies

- **Hard: `0003-13`.** It wrote `doc/reference/events.md`, which this task extends with every
  event type the later tasks add.
- **Hard: `0003-31`.** The typed-agent half of the concepts page describes `@outrig.agent`.
- **Hard: `0003-33`.** `/name` and the skills half of the concepts page need the directive.
- **Hard: `0003-34`.** The concepts page describes `outrig.Agent`, request channels and release,
  and the events reference gains the `agent.instance.*` family.
- **Hard: `0003-35`, `0003-36`.** The host-program usage page describes driving a class and
  injecting a live object, and the config reference the `[host]` grace key `0003-29` adds.
- **Hard: `0003-37`.** The concepts page and the events reference describe the two nudges, the
  cancel a child's unanswered request ends in, and the event that records it.
- **Soft: every earlier task**, each of which drops its own markers. This one is done after
  `0003-37` and before the tasks appended on 2026-10-28, which the second documentation pass
  (see Context) documents.

## See also

- `plan/phase/0003-python/README.md` -- the linked subsystems and why the MCP pages matter most.
- `crates/outrig-cli/src/mcp_self/docs.rs` -- the `include_str!` sites and the symlink test.

## Decisions

- **Fork 1: one runtime page.** `doc/concepts/python-runtime.md` carries executions, rounds,
  names, channels and waiting, child agents, `@outrig.agent`, `outrig.Agent` and skills, at 355
  lines; `doc/concepts/hosted-objects.md` beside it carries bindings. Neither needed splitting.
- **Fork 2: `subagents.md` gains the pointer.** It says the page is `run`'s, that `run-new`'s
  children are a different system, and that the width cap is `run`'s alone, bounded in `run-new`
  by `children-max` and `model-concurrency-max` instead.
- **The trust-model page is retitled "Trust Model"; its file and page key are not.** It now
  describes the boundary for everything in the container, MCP servers and the agent's Python
  alike, with "The agent cannot grow its own environment" as a section of its own. Keeping
  `mcp-trust-model.md` keeps every inbound link and the `concepts/mcp-trust-model` key `outrig
  mcp self` serves, which `list_includes_trust_model` asserts. `tests/mcp_self.rs`, which checks
  that `get_doc` returns the page by its heading, now looks for `# Trust Model`.
- **The new pages are not added to `outrig mcp self`'s bundle.** The bundle serves image-config
  design, and `[bindings.<name>]` is already in its config reference. Bundled pages link to the
  new ones by absolute `https://tgockel.github.io/outrig/` URL, as they already do for `run.md`
  and `llm-providers.md`, since `every_internal_link_resolves_to_a_registered_page` requires a
  relative link from one to reach another bundled page.
- **The host-program section moves from `run.md` to `doc/usage/host-programs.md`.** `run.md` is
  `run`'s page and keeps a pointer; `run_program_e2e.rs` reads its example from the new page. The
  moved text kept its wording but one fix: a read from piped input is `outrig run-new`'s, where it
  said `outrig run`.
- **The task's settings list for a host program's session (R7) says `[workspace]` applies; the
  code does not mount it.** `0003-29`'s `prepare` in `owner/session.rs` replaces the configured
  workspace with the program's `mounts=`, and `outrig.config()` shows the configured one. The new
  config section, "What a host program's session reads", says what the code does, and adds what
  the list left out, each read from the code: `[network]` (`LaunchSpec::from_config`), the
  top-level limits and tool limits (`Config::limits(None)`, `resolve_agent` with no agent), and
  that no MCP server, sidecar or skill starts.
- **Vocabulary is defined once, in `python-runtime.md#vocabulary`.** Pages about both loops --
  `config.md`, `llm-providers.md`, `events.md`, `cli.md` -- use `run-new`'s words and say so; pages
  about `run` alone -- `run.md`, `subagents.md`, `mcp-servers.md`, `recipes.md`, the quickstart --
  say that a turn there is a prompt and its work. A "turn" that quotes the program's own output,
  `turns are capped at 32768` in `config.md`, is left as the program prints it.
- **The Python owner API reference is generated, at the maintainer's choice.**
  `scripts/python-api-reference.py` reads the package with `ast`, imports nothing, and checks by
  default and writes with `--write`, as `check-public-api.py` does; CI's `doc-style` job runs the
  check. Every name in `__all__`, `_HOST_ONLY` and `_KERNEL_ONLY` must be placed on the page or
  listed as kernel-only, so a newly exported name fails rather than going unlisted. The types a
  program reads from other names' values -- `Model`, `ShutdownReport`'s parts, and `Handle`, which
  is not exported -- are listed under their bare names. `Schema` and `Handle` are shown without
  their `__init__`, which takes the engine's internals.
- **`events.md` keeps its categories.** `event.rs`'s section comments group the `agent.*` events
  under execution diagnostics and `binding.injected` under integration audit, the other way from
  the page. The page's placement decides what each event may hold, and the tasks that added them
  placed them there on purpose, so it stands; this task changes no event and no comment.
- **Three event entries had drifted from the code**, each since `0003-37` or before:
  `agent.request.cancelled` gains `reason`; `agent.request.settled`'s `cause` belongs to a
  submission the engine cancelled, carried by `RequestCancelled.__cause__`, not to a rejected
  one; and `message.sent`'s `from` can be a child's subject, which a `message.refused` follows.
- **`SECURITY.md` gains what the phase changed and it lacked**, checked against
  `plan/phase/0003-python/security.md` point by point. In scope: code in the container reaching
  the bindings process or a host program other than through the relay, a request to a binding
  running unpublished, or one admitted after the close; agent code starting a container, sidecar,
  MCP server or binding; and, under `run-new`, a credential from an MCP or sidecar block reaching
  the container. Known boundaries: the host program as trusted host code and the authority of a
  live object it injects; that the events are a record only under `[events] mode = "record"`;
  that a binding's grant covers OutRig's own files and is not limited by a read-only mount; and
  that agents in one session are not isolated from each other. Left out: the design's bounds on
  what the container sends and its note that what the host queues is not yet bounded, which are
  robustness rather than the boundary, and the supported-versions table stays at `0.2.x`, since
  0.3 is not released.
- **Every remaining `TODO: Incomplete` marker is still honest.** All seven describe work not
  done: `llm-providers.md` (other Rig styles, and prompt caching in `run-new`), `mcp-servers.md`
  (auto-restart and health checks), `usage/image.md` (the known MCP servers), `usage/config.md`
  and `usage/init.md` (non-interactive modes), `recipes.md` (sharing configs) and `run.md` (typed
  multi-line input for `run`). None was dropped.
- **Stale text found on the way, and fixed:** `cli.md` said podman forwards the host's proxy
  variables into `run-new`'s container, which 0.2.2's `--http-proxy=false` stopped; its exit-code
  paragraph left out a host program's `3`; `crates/outrig-cli/README.md` said the library avoids
  the LLM dependency graph; `_host.py`'s module docstring named `outrig run <script.py>`;
  `concepts/README.md` counted four concepts and said a subagent cannot launch subagents; and
  `0003-70`'s note said every `concepts/` and `reference/` page is a symlink.
- **Review corrections, each checked against the code.** Only `Outrig::launch` mounts the
  payload and reserves `/outrig`, so `containers.md` scopes both to `run-new`'s primary, a host
  program's and an embedder's; `run`, `run-legacy`, `outrig mcp` and sidecars get none of it. A
  standalone `@outrig.agent` declaration is called only in a kernel (`_call` stays `None` on the
  host), which its docstring now says. A handle raises an `outrig.AgentError` for what the child's
  work or the session ended; a typed call's raising `validate=`, a model not configured and work
  too long to send end it on the caller's side with other exceptions, which `python-runtime.md`,
  the `Handle` and `AgentError` docstrings and the reference's Errors introduction now say. And
  `bindings::approve` reads the approval store before it looks for an approver, so a host
  program's session starts a repository's binding approved before; the pages that said it
  refuses every one now say so, `an_approval_is_remembered_and_an_edit_asks_again` asserts it,
  and `plan/next/host-program-approves-repo-bindings.md` keeps only the approver a program lacks.
