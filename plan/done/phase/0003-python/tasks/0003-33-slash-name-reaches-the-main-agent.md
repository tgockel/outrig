# 0003-33 -- `/name` reaches the main agent as a call it makes

## Context

`skills.md` settles that a slash directive asks the main agent to do something; it is not a way
around the agent. What runs is a call the agent makes, in its own execution, in its own history,
counted against its round. So `/review-diff a.py b.py --max-parallel 2` reaches the agent as text
on its user channel, with the resolved skill named on the `Delivery`, and the agent's code calls
`outrig.skills.invoke("review-diff", text)`.

Turning the text into arguments is a model's work, done where a reader can find the result.
`invoke` derives a dataclass `<Skill>Params` from the entry's signature and runs a typed agent
call, `parse_params(text) -> <Skill>Params | Clarification` (`typed-agents.md`): a fresh child
reads the text with the entry's docstring and the parameter help as its instructions and
completes with an instance, which is decoded strictly, repaired when its shape is wrong, and
recorded on the invocation's event -- or with a `Clarification`, when the text does not determine
a required field or the mapping it would make widens the scope beyond what the text says, which
`invoke` raises to the agent as `SkillNeedsClarification` so that the agent can ask the user. An
agent that already understood the text, or that asked and now knows, calls
`invoke(name, params=<Skill>Params(...))`, and no model is called. Until 2026-10-02 this task
parsed the text deterministically from the entry's signature and had the agent write the call
itself when parsing failed; `skills.md` records that design and why it was dropped under
"Rejected alternatives". The clarification result was added on 2026-10-05, after the follow-up
review showed that a type-valid instance can still widen scope: `just the auth changes` as an
empty `paths`, which reviews every changed file.

A parameter named in `bind` is not a field of `<Skill>Params` at all. It is filled from the
session's binding with the host's reference -- never from the agent's variable of the same name,
which the agent may have rebound.

An instruction-only skill, one with no `skill.py`, needs none of this. Its directive delivers the
`SKILL.md` body followed by the line the user typed, as the Agent Skills standard has a skill's
body loaded when the skill is used, and the agent reads it as it reads any message.

`run-new`'s REPL matches `/help` and `/quit` in `crates/outrig-cli/src/cli/run_new/converse.rs`,
and any other `/word` is an unknown command today.

This is also the task where the phase's parts first run together. The phase's exit criteria ask
for a skill that uses a binding and runs typed children in parallel with one of them repaired,
working end to end through `run-new`, and this task's acceptance runs it.

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
  context on every directive, and the agent reads the body with `outrig.skills.read` (`0003-32`)
  when the parameter help is not enough. For an instruction-only skill, the body is its `SKILL.md`
  body followed by the line as typed, read through the skill's source, with the skill named the
  same way. The embedding API gains a way to send a message naming a skill, so an embedder's front
  end can send a directive.
- **The preamble says what such a message asks**: for a skill with a `skill.py`, call
  `outrig.skills.invoke(name, text)` with the text after the name; and when that raises
  `SkillNeedsClarification`, put its `question` to the user on the user channel and call again
  with `params=` once the answer is in.
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
  derived. If `0003-32`'s block parser reports `parameters` as an unknown key, this task teaches it
  the key.
- **`outrig.skills.invoke(name, text, *, reload=False)`** imports the skill, derives
  `<Skill>Params`, runs `parse_params` on `text` -- raising `SkillNeedsClarification` when it
  returns a `Clarification`, below -- injects the bound parameters, and calls the entry with the
  instance's fields as the signature asks: positional parameters positionally in the
  signature's order, with a bound one filled in its position, a `*args` field unpacked, and
  keyword-only parameters by name. A `def` entry is called, an `async def` one awaited. `invoke`
  returns a handle, as a typed call does: the invocation starts when `invoke` is called, `await`
  gives the entry's return value or raises what it raised, and `h.cancel()` ends it -- the parser
  child released, or the running entry interrupted. Whether the handle is `work.md`'s type or a
  narrower one is this task's.
- **`parse_params` is a typed agent call**, declared by the runtime per skill with `@outrig.agent`
  (`0003-31`): one input, `text: str`, bound into the child's namespace and never interpolated
  into the instructions; the result type `<Skill>Params | Clarification`; and a docstring made of
  the runtime's fixed instruction -- that `text` is what the user typed after `/<name>`, exact
  options or prose; that the child completes with the instance it means when the text determines
  one, a plain reading within the words included; and that it completes with a `Clarification`
  when the text does not determine a required field or the mapping it would make widens the scope
  beyond what the text says -- followed by the entry's docstring. The child is fresh, on the
  calling agent's model -- the session's, when the main agent calls `invoke` -- and is released
  when the call settles. Decoding, repair and the attempt limit are `typed-agents.md`'s: a
  completion of the wrong shape is raised in the child with its problems and the child completes
  again, and past the limit the call settles with `CompletionRejected`, which `invoke` raises to
  the agent with nothing called. The parser's liveness is `work.md`'s rule for a work child: a
  round that ends without a completion gets one further round from the host, asking for the
  result by the request's id, and if that round ends the same way the call settles with
  `CompletionRejected`, whose reason is that the child ended without completing, and the child is
  released; the parser never idles with the call open. The child counts against `children-max`
  (`0003-30`), so a launch past it raises `AgentLimitReached` from `invoke` at once, and its usage
  is attributed to it and added to the invocation and the round. Fork 1 is whether a skill may
  name a model for it.
- **`Clarification` and `SkillNeedsClarification`**, both in `outrig.skills`. `Clarification` is a
  `@dataclass(frozen=True, kw_only=True)` with one field, `question: str`, the parser's second
  result. The union decodes as `0003-26`'s rule has it: a completion is taken only when exactly
  one member accepts it. The members are told apart by their keys, so a `<Skill>Params` field
  named `question` would make them overlap, and the derivation fails with `signature-unsupported`
  naming it, before any child is spawned. When the parser completes with a `Clarification`, the
  parse call settles with it, the child is released, and `invoke` raises
  `SkillNeedsClarification`, an `outrig.AgentError` whose `question` attribute is the child's,
  with nothing called and no `skill.invocation.started`; the parse call's settled event carries the
  invocation's id and the `Clarification`. The agent asks on its user channel and calls `invoke`
  again with `params=`, which makes no parse call, or with new text, which parses again; the
  second call is a new invocation with an id of its own.
- **`invoke(name, params=<Skill>Params(...))`** makes no model call and spawns no child, after a
  `SkillNeedsClarification` as at any other time. `params` is checked against the field types as a
  typed call checks its inputs, and anything that is not an instance of this skill's class as
  currently derived -- another skill's instance, a plain `dict` -- is refused before anything is
  called. Exactly one of `text` and `params` is given, and `params` with `reload=True` is refused,
  since the reload would derive a new class and the instance given is of the old one; the caller
  reloads with `outrig.skills.reload`, takes the new class, and constructs again. Bound parameters
  are injected and the invocation's events emitted as for a parsed call. The class is reached as
  fork 2 settles. A skill parameter named `reload` is a field like any other, since `invoke`'s
  controls are its own keywords and the skill's parameters are the instance's fields.
- **Failures before the call**: `missing-binding` for a `bind` entry naming a binding the session
  lacks, `entry-not-found` for an `entry` the module does not define -- which discovery cannot check
  without running Python -- `signature-unsupported` as above, and `unknown-skill`. Each is raised
  before anything is called and before any child is spawned.
- **`skill-changed` at `invoke`**, checked against `0003-32`'s digest. `reload=True` reloads first,
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
  events carry the invocation's id and the rejection. A parse that completes with a
  `Clarification` ends it at the same point, the parse call's settled event carrying the
  invocation's id and the `Clarification`. The invocation is the link between a call
  and the main agent's round that `0003-30`'s spend chain leaves for this task: a child's usage is
  added to the invocation it ran under, and from there to the round (`typed-agents.md`).
- **Correlation.** While an invocation runs -- from the call to `invoke`, so the parse call is
  inside it -- the boundary events of the hosted calls made in it and the start events of the
  typed-agent calls it makes carry the invocation's id, through a context variable `invoke` sets
  around the entry, which tasks the entry starts and `to_thread` workers inherit; a hosted call
  made on the entry's own thread runs in its context already. It is diagnostic context, not a
  principal: nothing decides by it, and code in the same interpreter can set or clear it
  (`skills.md`). A skill's helper called directly -- its module imported and the function called,
  without `invoke` -- produces the boundary events of its hosted calls without the id, and no
  invocation events.

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
  receives names the four fields, their types and their help, and `Clarification` with its
  `question`.
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
- **A parser that ends without completing is rejected after one further round.** The mock ends
  the parser's first round without a completion, and the one round the host then opens for it the
  same way: `await invoke(...)` raises `CompletionRejected` saying the child ended without
  completing, no third round is opened, the child is released -- the record shows no resident
  parser -- the entry is not called, and no `skill.invocation.started` is emitted.
- **The parser may ask.** With `/review-diff just the auth changes` and the mock completing
  `{"question": "Which paths are the auth changes?"}`: `await invoke(...)` raises
  `SkillNeedsClarification` whose `question` is that string, and `except outrig.AgentError`
  catches it; the entry is not called, no `skill.invocation.started` is emitted, and the parse
  call's settled event carries the invocation's id and the `Clarification`. The call that follows,
  `invoke("review-diff", params=ReviewDiffParams(paths=["src/auth/", "tests/test_auth.py"]))`,
  spawns no child and makes no model request, calls the entry as
  `main(<repo>, "origin/main", "src/auth/", "tests/test_auth.py", max_parallel=4,
  allow_partial=False)`, and its `skill.invocation.started` carries an invocation id of its own and
  says the caller passed the instance.
- **A completion that neither member takes is repaired, and `question` is refused.** The mock
  completes `{"question": 5}` first and `{"paths": ["a.py"]}` second: one `agent.request.invalid`
  naming `$.question`, then the entry runs once with `"a.py"`. A fixture entry
  `def main(question: str = "")` fails `invoke` with `signature-unsupported` naming `question`,
  before any child is spawned.
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
- With a skill named `help` present, `/help` runs the built-in, and the skill was reported when
  the catalog was read.
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
  made from a task it starts, one made inside a callback of a hosted call it makes in a
  `to_thread` worker, and
  an `@outrig.agent` call it makes carry the invocation's id in their events. The same helper
  called directly, after `import outrig_skills.review_diff.skill`, produces the hosted call's
  boundary events without an invocation id, and no invocation events.
- **The parts compose, end to end**, through `run-new` with a mock model and a binding of
  `0003-16`'s fixture library. A `/name` directive is parsed by a child into `ReviewDiffParams`,
  and the entry uses its bound binding and makes three `@outrig.agent` calls, which return handles
  that `asyncio.gather` accepts, one of whose children completes with a wrong shape once and is
  repaired.
  The events' parentage matches the run: the parse call and each child call under the invocation,
  the repair under its call, and each boundary event under its execution, the entry's carrying the
  invocation's id. Each child's usage is in its own events and in its call's, the invocation's and
  the round's totals, once each.
- **Shutdown with typed children running**: the same run, shut down while the three calls are
  under way, settles each call's waiter with the documented error, lists every child's work and
  every hosted call in the `ShutdownReport`, and returns within the test's deadline.
- `crates/outrig/public-api.txt` regenerated: the way to send a message naming a skill, the
  invocation events, and the invocation's id on the events that carry it are its only additions.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **A per-skill parse model -- Recommended: the session's model, with `parse-model` as the
   alternative.** The parse is a small job, and a key such as `parse-model = "fast"` under
   `[tool.outrig.skill]`, naming a model from the operator's configuration as
   `@outrig.agent(model=)` does, would let a skill serve it with a cheaper model. Nothing has
   measured how well a cheaper model fills a `<Skill>Params`, and a repository key that picks a
   model picks spend, which #287 is about constraining; so the parser runs on the calling agent's
   model until a measurement says otherwise. If the key is taken, it selects among configured
   models only, and an unknown name fails `invoke` before any child is spawned.
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

- **Hard: `0003-22`.** Injection passes a binding's host reference, which needs bindings in the
  kernel.
- **Hard: `0003-31`.** The parse is a typed agent call: `parse_params(text) -> <Skill>Params |
  Clarification` is declared with `@outrig.agent`, its child is spawned and released as a
  decorated call's is, and its completion is decoded strictly -- the union by `0003-26`'s rule,
  through it -- and repaired by the decorator's path, so no `invoke(name, text)` works without
  it. The end-to-end acceptance also runs typed children
  through it. Through it, `0003-30`'s children, `children-max`, and the spend chain the invocation
  joins.
- **Hard: `0003-32`.** The catalog the REPL matches against, the loader, the digest and
  `outrig.skills.read` are its.

## See also

- `plan/phase/0003-python/skills.md` -- the directive, `<Skill>Params`, the typed parse, bind
  injection, and the deterministic parse it rejected.
- `plan/phase/0003-python/typed-agents.md` -- the call `parse_params` is one of: inputs, strict
  decoding, repair and `CompletionRejected`.
- `plan/phase/0003-python/work.md` -- the one further round a work child gets, and the
  `CompletionRejected` that settles its call when that round ends the same way.
- `plan/phase/0003-python/potential/exact-skill-directives.md` -- exact directives dispatched
  without a model, the alternative not shipped.
- `plan/phase/0003-python/messages.md` -- the `Delivery` this adds a field to.
- `crates/outrig-cli/src/cli/run_new/converse.rs` and `crates/outrig-cli/src/repl.rs` -- where
  commands are matched and help is composed.
- `plan/next/children-have-a-user-channel.md` -- a child-addressing REPL form must not collide with
  `/name`.

## Decisions

Forks 1, 2 and 3 were taken as recommended: the parser runs on the calling agent's model, with no
`parse-model` key; `outrig.skills.params(name)` returns the class; and a reload while an
invocation of the skill runs, in any kernel still open, is refused with `skill-busy`. What no doc
settled is recorded here.

- **`agent.call.settled` gains `value` (the maintainer's call).** The acceptance asks that the
  parse call's settled event carry the `Clarification`, and also that `public-api.txt` gain only
  the directive API, the invocation events and the invocation's id. The two disagree:
  `AgentCallSettled` had no field for a result. It now has `value`, a preview of the result a
  call was accepted with -- every typed call's, not the parser's alone -- `None` when it was not
  accepted or the session's events are not observed. It is the one public addition outside the
  list; the same JSON is also `agent.request.replied`'s `value`.
- **`entry`, `bind` and `parameters` come from the host.** Discovery's reader of the `# ///
  script` block already parsed `[tool.outrig.skill]`; it now keeps the three keys in the skill's
  metadata, re-read whenever its digest moves, and the `skill.load` answer carries them to the
  package's loader. The interpreter parses no TOML, so one parser decides what a block says.
  `parameters` is a table of strings, as `bind` is, and anything else is `metadata-invalid`.
- **`skill-changed` at `invoke`** is a new request, `skill.check {skill, digest}`, which lists the
  skill again and refuses a digest that moved: a package imported earlier might fetch no file
  during the invocation, so nothing else would notice.
- **What can be found before anything runs raises from `invoke` itself**, as a typed call's
  argument check raises from the call: the argument checks, `unknown-skill`, the import and its
  failures, `skill-changed`, `skill-busy` under `reload=True`, the derivation's
  `signature-unsupported` and `entry-not-found`, the `params` checks and `missing-binding`. No id
  is spent and no event is published for them. What the run decides -- the parse's
  `CompletionRejected` or `SkillNeedsClarification`, a limit, what the entry raises, a cancel --
  comes from awaiting the handle. An instruction-only skill invoked is `entry-not-found`, saying
  `outrig.skills.read` returns its instructions.
- **The handle is narrower than `work.md`'s: `outrig.skills.Invocation`**, with `id`, `skill`,
  `await`, `future`, `done()`, `result()` and `cancel()`, and no `status` or `progress`, which
  describe a child's request. `runtime.wait` takes it as it takes a `Handle`. It owns a future of
  the calling kernel's loop, settled once, by whichever comes first of the run's `finally` and a
  done-callback for a task cancelled before its first step, whose `finally` never runs -- never
  the task's own state, a cancelled task's being one a view cannot copy. `cancel()` is safe from
  any thread and has `Task.cancel`'s meaning: a parse in progress is cancelled, which releases its
  child; an `async def` entry gets `CancelledError` at its next `await`; a `def` entry runs on the
  loop to its end, as any synchronous call does. A cancelled invocation raises
  `outrig.RequestCancelled`, never `CancelledError`, which an awaiting execution would take for
  its own cancellation. A `KeyboardInterrupt` or `SystemExit` landing in the entry settles the
  handle with it and is raised on to the loop, as for any task. The run's `finally` awaits
  nothing: a released child's loop gives its tasks a second.
- **The parameter class.** Only the annotation of a parameter that becomes a field is resolved, one
  at a time, as `typing.get_type_hints` resolves one, in the globals of the function the signature
  comes from -- through what decorators wrap and what a `functools.partial` holds, as
  `inspect.signature` follows them, so an entry made with `functools.wraps(partial(...))` derives --
  and `{}` when that is no function: `from __future__ import annotations` works, and a return
  annotation or a bound parameter's, which may name what the skill imports only under
  `TYPE_CHECKING`, is never evaluated. Both were found in review: resolving every annotation refused
  such a skill, and reading `__globals__` of what `inspect.unwrap` gave raised `AttributeError` for
  a partial. A signature that cannot be read, such as a wrapper loop, is `signature-unsupported`. An
  entry must be a function. Each field's type is checked with its markers stripped, before the class
  is built and before any model is asked; a default must be of its field's type, as a typed call's
  parameter's must, so `x: int = None` is `signature-unsupported` rather than an instance that fails
  its own check; an unhashable default -- which a dataclass refuses as a field default -- is kept,
  shared, through a factory returning it; `*args` defaults to a fresh list. Help from
  `[tool.outrig.skill.parameters]` replaces a `doc()` the annotation already has, which the schema
  would otherwise refuse as two. A field whose JSON key is `question`, an alias's included, is
  refused, since the union could not tell the two results apart. A `parameters` key naming no field
  is a `warnings.warn` in the execution that derived the class -- "reported", where the task says
  other mistakes fail -- and the class is `<Camel>Params` in the entry's module. It is derived once
  per import of the package, outside any lock, since deriving can import and a skill's import could
  call `params()`; the first of two kernels deriving at once is the one kept.
- **The parser is declared as `@outrig.agent` declares a function**, through `_agents._declare`,
  and called through `_agents._call`, as the decorator's wrapper calls it, `invoke` having checked
  the text is a `str`. The declaration takes the entry module as its `__module__` and
  `parse_params` as its qualname, so a parse that asks or is refused -- which publishes no
  invocation event -- still names the skill on its call's events, and its digest is the SHA-256 of
  the entry module's source, the digest any declaration in that module carries. Its instructions
  are `_PARSE` in `outrig/skills.py`, formatted with the skill's name and its class's and never
  with the text, followed by the entry's cleaned docstring, or the skill's description when it has
  none.
- **Correlation.** A `ContextVar` holds the ids of the invocations the calling code runs inside,
  innermost first, and `outrig.skills._current()` keeps those still in `_running`, the registry of
  invocations not yet ended -- the same one `reload` reads for `skill-busy` -- so a task an entry
  started and left running stops naming the invocation once it ends. A
  spawn line carries the whole list, a typed call takes the innermost id when it is made, and an
  RPyC request's label carries the innermost. `_Wait` takes it on the calling thread, as it takes
  the execution, since the notice of a call never sent is written from the reader thread. As with
  `execid`, only `hosted.received` names it; the request's other events share its id. Threads
  started with `threading.Thread`, pool work items and `asyncio.to_thread` carry the context, as
  they do the execution's; `_thread.start_new_thread` does not.
- **Spend is read from the tree when the invocation ends**, not accumulated: each child's node
  records the invocations its spawn was made inside, nodes stay in the registry after a release,
  and each node's slot keeps every attempt, so `agent.invocation.ended` sums the attempts of the
  nodes holding the id and of every agent below them, each once. The ledger and the rounds are
  unchanged, so the main round counts each attempt once, as before. A child made inside an
  invocation made inside another counts toward both. What a child spends after the entry ended
  is in the rounds' totals and not on `skill.invocation.finished`.
- **The protocol lines are `agent.invocation` and `agent.invocation.ended`**, routed to the
  children's registry, which holds the slots; they are published as `skill.invocation.started` and
  `skill.invocation.finished`. The second is sent only when the entry was called, so a parse that
  ends the invocation publishes neither. `by` on the started event is `parser` exactly when it
  names the parse call.
- **`UserChannel::send_directive(skill, line)`.** For a skill with a `skill.py` the message is the
  line, queued when the call returns, as `send`'s is. For one without, it is the skill's
  instructions, read through its source within the session's places and bound, trimmed, a blank
  line, then the line, queued once the future has read them. An unknown skill is an error and
  nothing is sent. The `msg` line names the skill, which the interpreter checks is a string, and
  `skill.directive.received` follows the message's `message.sent`, carrying its id. The catalog
  reaches the channel through `Holdings`, keeping `Agent::build` at seven parameters.
- **The REPL.** `drive` shows the events already queued before it reads a line, which is where
  `skill.discovered` is as the session starts, so a `/name` typed at once finds its skill; a skill
  named `help` or `quit` is left out of what a directive reaches, so `/help x` is still an unknown
  command. A directive's name follows the slash directly, and any whitespace ends it, which is where
  the preamble's `(d.body.split(maxsplit=1) + [""])[1]` splits too, so the parser of
  `/review-diff<TAB>a.py b.py` is given `a.py b.py` (found in review: the `partition(" ")` the
  design showed split only at a space). While a directive is being queued no other line is taken,
  and its answer starts a round when none runs -- one that finds nothing new if the directive was
  refused: a directive on piped input's last line still runs, and a line typed after an
  instruction-only directive cannot reach the agent first. `/help` lists `/<skill> [text]` when the
  session has skills, and a skill named `help` or `quit` is said once, as the catalog is read.
- **`help(outrig.skills)` stays within the kernel's 8 KiB.** `__all__` is the functions and
  `SkillError`; `Invocation`, `Clarification`, `SkillNeedsClarification`, `Origin` and `Skill` are
  public and named in the module's docstring, each with `help()` of its own -- an exception class
  alone renders past 2 KiB with what it inherits.
- **Tests.** The session-level cases are `agent/invoke_tests.rs`, over `children_tests`'
  session with a builder hook for skill sources and the binding's install places; the fixture
  library installs into a directory kept between runs, as the test wheels are. `outrig-cli`'s
  tests gained a mock that routes by the system prompt, as the crate-private one does.
- **Of two implementations compared for complexity** (`/simplify`), the handle, the registry and
  the REPL's catalog are the alternative's: one class where this one had a handle over a run
  record, a flat registry by id where it had sets by skill, ids in the context where it had a chain
  of records, and one read of the catalog before the loop. The derivation stayed this one's: the
  alternative derived the class twice when two kernels invoked a skill at once, and stacked two
  `doc()` markers on a parameter whose annotation had one.
- **Follow-up.** Renaming `agent.call.settled` and `agent.request.settled`, whose names do not say
  what happened, is #718, at the maintainer's request.
