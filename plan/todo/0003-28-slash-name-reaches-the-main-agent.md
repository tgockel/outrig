# 0003-28 -- `/name` reaches the main agent as a call it makes

## Context

`skills.md` settles that a slash directive asks the main agent to do something; it is not a way
around the agent. What runs is a call the agent makes, in its own execution, in its own history,
counted against its round. So `/review-diff a.py b.py --max-parallel 2` reaches the agent as text
on its user channel, with the resolved skill named on the `Delivery`, and the agent's code calls
`outrig.skills.invoke("review-diff", text)`.

Turning the text into arguments is a model's work, done where a reader can find the result.
`invoke` derives a dataclass `<Skill>Params` from the entry's signature and runs a typed agent
call, `parse_params(text) -> <Skill>Params` (`typed-agents.md`): a fresh child reads the text with
the entry's docstring and the parameter help as its instructions and completes with an instance,
which is decoded strictly, repaired when its shape is wrong, and recorded on the invocation's
event. An agent that already understood the text calls `invoke(name, params=<Skill>Params(...))`,
and no model is called. Until 2026-10-02 this task parsed the text deterministically from the
entry's signature and had the agent write the call itself when parsing failed; `skills.md`
records that design and why it was dropped under "Rejected alternatives".

A parameter named in `bind` is not a field of `<Skill>Params` at all. It is filled from the
session's binding with the host's reference -- never from the agent's variable of the same name,
which the agent may have rebound.

An instruction-only skill, one with no `skill.py`, needs none of this. Its directive delivers the
`SKILL.md` body followed by the line the user typed, as the Agent Skills standard has a skill's
body loaded when the skill is used, and the agent reads it as it reads any message.

`run-new`'s REPL matches `/help` and `/quit` in `crates/outrig-cli/src/cli/run_new/converse.rs`,
`0003-22` adds `/approve` and `/deny`, and any other `/word` is an unknown command today.

This is also the task where the phase's parts first run together. The phase's exit criteria ask
for a skill that uses a binding, runs typed children in parallel with one of them repaired, and
meets an escalated call, working end to end through `run-new`, and this task's acceptance runs it.

## Goal

A user types `/name text`, the main agent makes the call it asks for, and the call is in the
agent's history like anything else it ran.

## Deliverables

- **The REPL recognizes `/name`** for each skill in the catalog. Built-in commands win, and a
  skill whose name a built-in takes is reported once, when the catalog is read; it stays reachable
  through `invoke`. A `/word` that is neither is still an unknown command.
- **Delivery as ordinary user-channel text.** For a skill with a `skill.py`, the line as typed is
  the body of a user-channel message, a `str`, and the `Delivery` names the resolved skill in a new
  optional field -- `None` on every other message; the field's spelling is this task's, and
  `messages.md` suggests `skill`. The `SKILL.md` body is not delivered with it: that would cost
  context on every directive, and the agent reads the body with `outrig.skills.read` (`0003-27`)
  when the parameter help is not enough. For an instruction-only skill, the body is its `SKILL.md`
  body followed by the line as typed, read through the skill's source, with the skill named the
  same way. The embedding API gains a way to send a message naming a skill, so an embedder's front
  end can send a directive.
- **The preamble says what such a message asks**: for a skill with a `skill.py`, call
  `outrig.skills.invoke(name, text)` with the text after the name.
- **`<Skill>Params`, derived from the entry's signature** when the skill is loaded for `invoke`: a
  `@dataclass(frozen=True, kw_only=True)` named for the skill in CamelCase, `ReviewDiffParams` for
  `review-diff`. A parameter named in `bind` is not a field. Every other positional-only,
  positional-or-keyword and keyword-only parameter is a field of its name, typed by its annotation
  -- `str` when it has none -- with its default when it has one and required otherwise; `*args: T`
  is a field of the parameter's name typed `list[T]` with an empty default; `**kwargs` fails the
  derivation with `signature-unsupported`, since there is no closed set of names to make fields
  from. Each field's type must be one `outrig.schema` takes (`typed-agents.md`), and any other
  annotation fails the derivation with the same code, naming the parameter, before any child is
  spawned. The class is derived once per load and kept with the skill's record, so a reload
  derives it again.
- **`[tool.outrig.skill.parameters]`**, a table in the `# /// script` block mapping a parameter's
  name to its help text, which becomes the field's `doc` and so its `description` in the schema
  text the parser child receives. `invoke` reads it in the interpreter from the source the loader
  holds, with the specification's regular expression and `tomllib`, or the loader records it on
  `__outrig_skill__`; which is this task's. A key naming no field is reported when the class is
  derived. If `0003-27`'s block parser reports `parameters` as an unknown key, this task teaches it
  the key.
- **`outrig.skills.invoke(name, text, *, reload=False)`** imports the skill, derives
  `<Skill>Params`, runs `parse_params` on `text`, injects the bound parameters, and calls the entry
  with the instance's fields as the signature asks: positional parameters positionally in the
  signature's order, with a bound one filled in its position, a `*args` field unpacked, and
  keyword-only parameters by name. A `def` entry is called, an `async def` one awaited. `invoke`
  returns a handle, as a typed call does: the invocation starts when `invoke` is called, `await`
  gives the entry's return value or raises what it raised, and `h.cancel()` ends it -- the parser
  child released, or the running entry interrupted. Whether the handle is `work.md`'s type or a
  narrower one is this task's.
- **`parse_params` is a typed agent call**, declared by the runtime per skill with `@outrig.agent`
  (`0003-26`): one input, `text: str`, bound into the child's namespace and never interpolated
  into the instructions; the result type `<Skill>Params`; and a docstring made of the runtime's
  fixed instruction -- that `text` is what the user typed after `/<name>`, exact options or prose,
  and the child completes with the instance it means -- followed by the entry's docstring. The
  child is fresh, on the calling agent's model -- the session's, when the main agent calls `invoke`
  -- and is released when the call settles. Decoding, repair and the attempt limit are
  `typed-agents.md`'s: a completion of the wrong shape is raised in the child with its problems
  and the child completes again, and past the limit the call settles with `CompletionRejected`,
  which `invoke` raises to the agent with nothing called. The child counts against `children-max`
  (`0003-25`), so a launch past it raises `AgentLimitReached` from `invoke` at once, and its usage
  is attributed to it and added to the invocation and the round. Fork 1 is whether a skill may
  name a model for it.
- **`invoke(name, params=<Skill>Params(...))`** makes no model call and spawns no child. `params`
  is checked against the field types as a typed call checks its inputs, and anything that is not
  an instance of this skill's class as currently derived -- another skill's instance, a plain
  `dict` -- is refused before anything is called. Exactly one of `text` and `params` is given, and
  `params` with `reload=True` is refused, since the reload would derive a new class and the
  instance given is of the old one; the caller reloads with `outrig.skills.reload`, takes the new
  class, and constructs again. Bound parameters are injected and the invocation's events emitted
  as for a parsed call. The class is reached as fork 2 settles. A skill parameter named `reload`
  is a field like any other, since `invoke`'s controls are its own keywords and the skill's
  parameters are the instance's fields.
- **Failures before the call**: `missing-binding` for a `bind` entry naming a binding the session
  lacks, `entry-not-found` for an `entry` the module does not define -- which discovery cannot check
  without running Python -- `signature-unsupported` as above, and `unknown-skill`. Each is raised
  before anything is called and before any child is spawned.
- **`skill-changed` at `invoke`**, checked against `0003-27`'s digest. `reload=True` reloads first,
  as `outrig.skills.reload` does; it is a Python keyword of `invoke`, never a skill parameter. A
  reload while an invocation of the same skill runs, per fork 3.
- **Invocation events**, emitted by the runtime rather than by the skill -- the host when it
  delivers a directive, `invoke` for the rest -- in `0003-13`'s envelope:
  `skill.directive.received`, when the REPL resolves one; and, sharing the invocation's id,
  `skill.invocation.started`, when the entry is called, with the skill, its source, its digest,
  the `<Skill>Params` instance as a bounded value preview, whether a child parsed it -- with that
  call's id -- or the caller passed it, and the names of the bindings injected; and
  `skill.invocation.finished`, with whether the entry returned, raised or was cancelled, the
  duration, the result's type, and the model usage of the children the invocation ran, the parser
  included. A parse that settles with
  `CompletionRejected` ends the invocation before `skill.invocation.started`; the parse call's own
  events carry the invocation's id and the rejection. The invocation is the link between a call
  and the main agent's round that `0003-25`'s spend chain leaves for this task: a child's usage is
  added to the invocation it ran under, and from there to the round (`typed-agents.md`).
- **Correlation.** While an invocation runs -- from the call to `invoke`, so the parse call is
  inside it -- the boundary events of the hosted calls made in it and the start events of the
  typed-agent calls it makes carry the invocation's id, through a context variable `invoke` sets
  around the entry, which tasks the entry starts and `to_thread` workers inherit. It is diagnostic
  context, not a principal: policy never reads it, and code in the same interpreter can set or
  clear it (`skills.md`). A skill's helper called directly -- its module imported and the function
  called, without `invoke` -- produces the boundary events of its hosted calls without the id, and
  no invocation events.

## Acceptance

Against a skill named `review-diff` whose entry is the one `skills.md` uses, with `bind` mapping
`repo` to the binding `repo` and the `parameters` table `skills.md` shows:

```python
async def main(repo, base: str = "origin/main", *paths: str,
               max_parallel: int = 4, allow_partial: bool = False): ...
```

The model is a mock throughout, so a "parse" below is what the mock completes with; what a real
model makes of a directive is not measured here.

- **`ReviewDiffParams` has the expected fields**: `base: str = "origin/main"`, `paths: list[str]`
  defaulting to `[]`, `max_parallel: int = 4` and `allow_partial: bool = False`, in that order,
  each carrying its help from the table as `doc`, and no `repo`. The schema text the parser child
  receives names the four fields, their types and their help.
- **Every parameter kind derives.** A fixture entry
  `def main(a: int, /, b: str, c: float = 1.5, *rest: str, d: bool, e: Literal["x", "y"] = "x")`
  derives the fields `a: int`, `b: str`, `c: float = 1.5`, `rest: list[str] = []`, `d: bool` and
  `e: Literal["x", "y"] = "x"`, and `invoke(..., params=P(a=1, b="s", rest=["r"], d=True))` calls
  it as `main(1, "s", 1.5, "r", d=True, e="x")`.
- **`**kwargs` is refused**: a fixture entry with `**kwargs` fails `invoke` with
  `signature-unsupported` naming it, before any child is spawned -- no spawn event and no
  `agent.call.started` -- and the entry is not called.
- **The parse is a typed call.** `/review-diff a.py b.py --max-parallel 2 --allow-partial`, with
  the mock completing `{"base": "origin/main", "paths": ["a.py", "b.py"], "max_parallel": 2,
  "allow_partial": true}`, spawns one child whose `agent.call.started` carries the invocation's id
  and whose instructions hold the entry's docstring and the schema text; the entry is called as
  `main(<repo>, "origin/main", "a.py", "b.py", max_parallel=2, allow_partial=True)`; and
  `skill.invocation.started` carries `ReviewDiffParams(base="origin/main", paths=["a.py", "b.py"],
  max_parallel=2, allow_partial=True)` and the parse call's id.
- **The text is an input, not instruction text.** A directive whose text holds braces and the
  words of an instruction reaches the child as the variable `text`, unchanged, and the child's
  instructions do not contain it.
- **A wrong shape is repaired, once.** The mock completes `{"max_parallel": "2"}` first and the
  right shape second: one `agent.request.invalid` naming `$.max_parallel`, then the entry runs
  once with `max_parallel=2`.
- **A parse the child cannot complete calls nothing.** The mock completes the wrong shape three
  times: `await invoke(...)` raises `CompletionRejected` carrying the last value and its problems,
  the entry is not called, no `skill.invocation.started` is emitted, and the parse call's settled
  event carries the rejection under the invocation's id. The agent's `invoke` with `params=` that
  follows runs the entry and appears in `runtime.history` as an ordinary execution.
- **`params=` makes no model call**: `invoke("review-diff", params=ReviewDiffParams(paths=["a.py"],
  max_parallel=2))` calls the entry as `main(<repo>, "origin/main", "a.py", max_parallel=2,
  allow_partial=False)` with no spawn event, no `agent.call.*` event and no model request, and
  `skill.invocation.started` carries the instance and says the caller passed it.
- **`params` is checked**: `ReviewDiffParams(max_parallel="2")` is refused naming `max_parallel`,
  and an instance of another skill's class is refused naming the skill; neither calls the entry.
- **A user cannot set a bound parameter**: `ReviewDiffParams` has no `repo` field, so
  `ReviewDiffParams(repo=1)` is a `TypeError`, and a mock completion holding a `repo` key is
  rejected by strict decoding as a key that matches no field; the entry receives the binding.
- **Injection uses the binding after the agent sets `repo = None`**: the entry receives the hosted
  object, not `None`.
- **A skill parameter named `reload` is a field**: for a fixture entry with `reload: bool = False`,
  `invoke(name, params=P(reload=True))` passes `reload=True` to the entry and reloads nothing.
- A `bind` naming a binding the session lacks fails with `missing-binding`, and the entry is not
  called.
- **A `Literal` is an enum, and there is no place for a surplus value.** With a skill `tag` whose
  entry is `def main(name: str, kind: Literal["light", "annotated"] = "light")`, `TagParams`'s
  schema text has `kind` as an `enum` of the two strings and `name` as required; a mock completion
  with `"kind": "heavy"` is rejected naming the choices, and one with a key the class lacks is
  rejected as a key that matches no field; each is repaired before the entry runs.
- **An annotation outside what `outrig.schema` takes never runs**: a skill whose entry has a
  parameter annotated with a class whose constructor writes a file fails `invoke` with
  `signature-unsupported` when the class is derived, naming the parameter; no file is written and
  no child is spawned.
- **Failures before the call carry their codes**: an `entry` the module does not define fails with
  `entry-not-found`, and a name no source holds with `unknown-skill`; neither calls anything.
- **A reload during an invocation is refused** (with fork 3's recommendation):
  `outrig.skills.reload("review-diff")` while an invocation of it runs -- from inside its entry,
  and from another task while the entry awaits -- fails with `skill-busy`, and the invocation
  finishes on the code it started with.
- The typed line reaches the agent as a user-channel message whose `Delivery` names the skill, and
  nothing runs until the agent's code calls `invoke`.
- **An instruction-only skill's directive delivers its `SKILL.md` body followed by the line**,
  with the skill named on the `Delivery`, and the agent needs no `invoke` to read it. A directive
  for a skill with a `skill.py` delivers the line alone.
- With a skill named `approve` present, `/approve <id>` runs the built-in, and the skill was
  reported when the catalog was read.
- **Invocation events**: a directive the REPL resolves publishes `skill.directive.received`, and
  the agent's `invoke` publishes `skill.invocation.started` -- the skill, its source, its digest,
  the instance, how it was obtained and the injected binding names -- and
  `skill.invocation.finished`, sharing the invocation's id: returned, with the duration and the
  result's type; raised, for an entry that raises; and cancelled, for a handle cancelled while the
  entry runs.
- **Spend reaches the invocation**: the finished event of an invocation whose entry ran typed
  children carries the sum of their usage and the parser child's, and the main round's total
  includes it once.
- **Calls made in an invocation carry its id.** The parse call, a hosted call the entry makes, one
  made from a `to_thread` worker it starts, and an `@outrig.agent` call it makes carry the
  invocation's id in their events. The same helper called directly, after
  `import outrig_skills.review_diff.skill`, produces the hosted call's boundary events without an
  invocation id, and no invocation events.
- **The parts compose, end to end**, through `run-new` with a mock model, a mock evaluator and a
  binding of `0003-16`'s fixture library. A `/name` directive is parsed by a child into
  `ReviewDiffParams`, and the entry uses its bound binding; makes three `@outrig.agent` calls,
  which return handles that `asyncio.gather` accepts, one of whose children completes with a wrong
  shape once and is repaired; and makes a hosted call that an `evaluate` rule sends to the
  evaluator, which answers `escalate`, and that runs once `/approve` is typed.
  The events' parentage matches the run: the parse call and each child call under the invocation,
  the repair under its call, and each boundary event under its execution, the entry's carrying the
  invocation's id. Each child's usage is in its own events and in its call's, the invocation's and
  the round's totals, once each, and the evaluator's usage is in its own decision event and in
  none of those totals.
- **Shutdown with typed children running**: the same run, shut down while the three calls are
  under way and the escalated call waits for an answer, settles each call's waiter with the
  documented error, cancels the pending escalation, lists every child's work and every hosted
  call in the `ShutdownReport`, and returns within the test's deadline.
- `crates/outrig/public-api.txt` regenerated: the way to send a message naming a skill, the
  invocation events, and the invocation's id on the events that carry it are its only additions.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **A per-skill parse model -- Recommended: the session's model, with `parse-model` as the
   alternative.** The parse is a small job, and a key such as `parse-model = "fast"` under
   `[tool.outrig.skill]`, naming a model from the operator's configuration as
   `@outrig.agent(model=)` does, would let a skill serve it with a cheaper model. Nothing has
   measured how well a cheaper model fills a `<Skill>Params`, and a repository key that picks a
   model picks spend, which `plan/next/subagent-model-allowlist.md` is about constraining; so the
   parser runs on the calling agent's model until a measurement says otherwise. If the key is
   taken, it selects among configured models only, and an unknown name fails `invoke` before any
   child is spawned.
2. **How an agent reaches `<Skill>Params` -- Recommended: an accessor in `outrig.skills`, such as
   `outrig.skills.params("review-diff")`**, which loads the skill if needed and returns the class.
   The alternative is an attribute the loader sets on the entry module. The accessor keeps the
   module's namespace the skill's own and gives `help()` one place to point at; it is one more
   name in `outrig.skills`.
3. **A reload while an invocation of the same skill runs -- Recommended: refused with
   `skill-busy`.** Waiting instead would wait forever when the reload is asked for from inside
   that invocation, since the invocation cannot end while it waits. The invocation already running
   keeps the code it started with either way.

## Dependencies

- **Hard: `0003-21`.** Injection passes a binding's host reference, which needs bindings in the
  kernel.
- **Hard: `0003-22`.** `/approve` and `/deny` are built-ins a skill's name can collide with, and
  the end-to-end acceptance meets an escalated call.
- **Hard: `0003-23`.** The end-to-end acceptance's escalation comes from the evaluator, whose
  usage it checks is kept apart.
- **Hard: `0003-26`.** The parse is a typed agent call: `parse_params(text) -> <Skill>Params` is
  declared with `@outrig.agent`, its child is spawned and released as a decorated call's is, and
  its completion is decoded strictly and repaired by the decorator's path, so no
  `invoke(name, text)` works without it. The end-to-end acceptance also runs typed children
  through it. Through it, `0003-25`'s children, `children-max`, and the spend chain the invocation
  joins.
- **Hard: `0003-27`.** The catalog the REPL matches against, the loader, the digest and
  `outrig.skills.read` are its.

## See also

- `plan/phase/0003-python/skills.md` -- the directive, `<Skill>Params`, the typed parse, bind
  injection, and the deterministic parse it rejected.
- `plan/phase/0003-python/typed-agents.md` -- the call `parse_params` is one of: inputs, strict
  decoding, repair and `CompletionRejected`.
- `plan/phase/0003-python/potential/exact-skill-directives.md` -- exact directives dispatched
  without a model, the alternative not shipped.
- `plan/phase/0003-python/messages.md` -- the `Delivery` this adds a field to.
- `crates/outrig-cli/src/cli/run_new/converse.rs` and `crates/outrig-cli/src/repl.rs` -- where
  commands are matched and help is composed.
- `plan/next/children-have-a-user-channel.md` -- a child-addressing REPL form must not collide with
  `/name`.
