# Skills

A **skill** is reusable know-how that a repository, a user, or an embedding application supplies to
an agent: instructions in a `SKILL.md`, and optionally a Python module the agent imports and
calls. This page settles where skills come from, how they are listed without running any of their
code, how one loads as a module, and how a line like `/review-diff a.py b.py` becomes a call the
main agent makes. The design was settled in planning on 2026-09-30, and on 2026-10-02 the parse of
a directive's text was changed from a deterministic parser to a typed agent call ("Rejected
alternatives"). `0003-27` builds the sources, discovery, the loader and the preamble; `0003-28`
builds the directive, the parameter dataclass, the parse and injection.

Two rules govern the rest.

**A directive is not an execution path of its own.** `/name` reaches the main agent as a message,
and the agent's own code calls the skill, so every invocation is an execution in the agent's
history and its round, accounted like any other. Nothing runs a skill on the agent's behalf.

**Loading a skill grants nothing.** A skill runs with the authority of the agent that calls it,
and its metadata cannot add to that authority.

## Layout

```text
<repo-root>/.agents/
  outrig/config.toml       OutRig's repository config, as today
  skills/
    review-diff/
      SKILL.md             Agent Skills frontmatter, then instructions
      skill.py             optional: the module OutRig imports
      sections.py          optional: helpers, imported relatively
      data/ids.txt         optional: data, read with importlib.resources
```

A skill is a directory `<repo-root>/.agents/skills/<name>/`, beside the `.agents/outrig/` that
holds OutRig's repository config. `SKILL.md` carries the Agent Skills standard's frontmatter --
`name`, which must match the directory's name, and `description` -- followed by instructions in
Markdown. `skill.py` is optional, and a skill without one is **instruction-only**.

User skills come from `~/.agents/skills/<name>/`, in the same shape. **When the project and the
user both have a name, the project's skill is used, and discovery reports the user's as
shadowed.** A repository that redefines a name the user relies on is then visible rather than
silent.

**`.agents/skills` is not OutRig's directory.** Codex, Copilot, Cursor, Gemini CLI, OpenCode and
Amp read it. Claude Code reads `.claude/skills` instead, which is why this repository keeps
`groom-plan` and `next-task` in `.agents/skills` and makes `.claude/skills` a symlink to it. None
of those tools imports a skill's Python, so `skill.py` is OutRig's addition: another tool finds a
`SKILL.md` it understands and a file it does not use. There is no `.agents/outrig/skills/` root
("Rejected alternatives").

## Sources

**Every skill comes from a source on the host side**: the project's, the user's, or one an
embedder supplies through the session API (`embedding.md`). A source does two things: it lists the
skills it has, and it reads one file of one skill. The CLI supplies the project and user sources.
An embedder whose skills are held by a service of its own supplies a source that reads from that
service.

**Decided in planning (2026-09-30): files are fetched over the interpreter pipe when they are
used, not mounted into the container up front.** The reason is the embedder's catalog. Mounting
would put every skill into every container whether the session used it or not, and a catalog a
service holds has no directory to mount in the first place. Fetching on use serves the three kinds
of source the same way, user skills included, which are in a home directory the container does
not otherwise see.

Two consequences follow.

- **A skill reads its own data through `importlib.resources`** --
  `importlib.resources.files(__package__) / "data" / "ids.txt"` -- and not through a path built
  from `__file__`. A skill from the user's home or from an embedder's service has no file in the
  container, and one from the project is not read from the workspace either.
- **A source serves only files inside the skill's directory.** The host reads them with the user's
  authority, so a symlink in a repository's skill that resolves outside the skill -- to `~/.ssh`,
  say -- would hand a host file to the container. The project and user sources refuse it, and
  bound the number and size of the files they serve.

## Discovery runs no Python

Listing skills and resolving `/name` read metadata on the host. No skill code is imported, and no
Python runs on either side. Listing something never runs it, which is the rule `discovery.md`
applies to the inventory, and a skill's module body is code nobody has asked to run.

For each skill a source lists, discovery reads two things.

**`SKILL.md`'s frontmatter.** `name` must match the directory's name and follow the standard's
rule -- 1 to 64 lowercase letters, digits and hyphens, with no hyphen first, last, or next to
another -- and `description` must be present. A skill that fails either is reported as
`metadata-invalid` and left out. Parsing frontmatter with a YAML crate or with a subset that takes
only scalars is `0003-27`'s fork. Either way the parser reads untrusted text on the host, so it
refuses YAML anchors and aliases, or limits how far they expand, and bounds the size of its input
and the time it spends parsing; that requirement is part of the same fork.

**The `# /// script` block in `skill.py`**, the inline script metadata of PEP 723, now a PyPA
specification. Only the standardized `script` type is read, since the specification says tools
"MUST NOT read from metadata blocks with types that have not been standardized", and a file with
more than one `script` block is an error, `duplicate-metadata-block`, as the specification
requires. A block that is opened and never closed is ignored, which the specification also
requires. The block is found with the specification's canonical regular expression and parsed as
TOML.

OutRig adds a rule of its own: the block must come before the module's first statement, so every
line above its closing `# ///` is blank or a comment. Text shaped like a block inside a later
docstring or string is not read -- the specification leaves such text to each tool -- and finding
the block stays a scan of lines, with no Python parsed.

OutRig's keys are in the `[tool.outrig.skill]` table:

```python
# /// script
# requires-python = ">=3.13"
# dependencies = ["unidiff>=0.7"]
#
# [tool.outrig.skill]
# entry = "main"
# bind = { repo = "repo" }
#
# [tool.outrig.skill.parameters]
# base = "The ref to diff against."
# paths = "Paths that limit the diff; none means every changed file."
# max_parallel = "How many sections are reviewed at once."
# allow_partial = "Return a partial outcome instead of raising when a section fails."
# ///
```

- `entry` names the entry function, `main` by default. It may be a `def` or an `async def`.
- `bind` maps an entry parameter to a binding the session has (`hosted-objects.md`): here,
  parameter `repo` receives binding `repo`. See "Bound parameters" below.
- `parameters` maps an entry parameter to its help text. The text becomes the field's `doc` in the
  dataclass `invoke` derives from the entry's signature, and so part of what the child that parses
  a directive reads ("How `invoke` parses"). A parameter without an entry has its name and type
  alone.

An unknown key in `[tool.outrig.skill]` is reported as a warning rather than ignored.
`requires-python` and `dependencies` are the specification's own keys, checked when the skill
loads.

`[tool]` has `pyproject.toml`'s semantics, under which `[tool.<name>]` belongs to whoever owns
`<name>` on PyPI. Nobody has registered `outrig` there, so OutRig's claim to `[tool.outrig]` is
nominal; registering it is a separate decision for the maintainer.

Discovery does not read the entry's signature, since that means parsing Python and the host does
not. The parameters become known when the module is imported, which is when `invoke` derives
`<Skill>Params` from them -- so a key in `parameters` that names no parameter, or an entry whose
signature cannot become a dataclass, is reported then, not when the skill is listed.

## The preamble

The Agent Skills standard loads each skill's name and description at start and its body only when
the skill is used. The preamble does the same: it lists every skill by name and description, and
says that a message the user sent as `/name` asks the agent to run that skill with
`outrig.skills.invoke`, or, for an instruction-only skill, to follow the `SKILL.md` body the
message carries. Both halves of `discovery.md`'s test for the preamble hold. The agent
cannot learn the catalog by looking, since no skill file is in its container before it is used,
and a directive can arrive in any round.

Bodies are not in the preamble. An instruction-only skill's body enters the context when a
directive delivers it, or when the agent reads it because it judged the skill relevant (the
accessor is `0003-27`'s). An executable skill's parameters are learned from `help()` on its
`<Skill>Params` class, which lists each field with its type, default and help, and from `help()`
on its module once loaded.

## A skill is a module

From the agent's side a skill is a Python package.

- **Its name is `outrig_skills.<normalized-name>`**, hyphens becoming underscores: `review-diff`
  is `outrig_skills.review_diff`, and its entry module is `outrig_skills.review_diff.skill`. The
  standard's name rule makes the mapping one-to-one. A name that is not an identifier once
  normalized -- `7zip`, or the keyword `import` -- loads through `importlib.import_module`, which
  `invoke` uses, and cannot be written in an `import` statement.
- **Its files arrive over the pipe.** A finder on `sys.meta_path` serves `outrig_skills` and the
  skills in the catalog, and nothing else. Importing one of a skill's modules asks the host, which
  asks the skill's source. The loader also answers `get_source` and supplies a resource reader, so
  tracebacks show a skill's lines, `inspect.getsource` works on its functions, and
  `importlib.resources` reads its data.
- **Relative imports work, and nothing is shadowed.** `from . import sections` imports a sibling.
  Absolute imports use the ordinary `sys.path`, and the skill's directory is never added to it, so
  a skill's `json.py` is `outrig_skills.review_diff.json` and every other `import json` still gets
  the standard library's.
- **One module object serves every kernel.** `sys.modules` is interpreter-wide, so each co-hosted
  agent and each child shares one module per skill, and its module-level state with it.
  Per-invocation state belongs in locals. This is the sharing `agent-placement.md` describes, not
  isolation.
- **Invoking adds nothing to the agent's namespace.** No name is bound in the agent's globals. The
  agent can still `import outrig_skills.review_diff.skill as rd` and call helpers directly, which
  is ordinary Python: it skips `invoke`'s parse, its injection and its invocation events, and does
  not skip the boundary policy a hosted call goes through.

### Changed files and reload

A loaded skill records its origin on the module as `__outrig_skill__`: its name, its source, and
the digest of its files when they were loaded. `invoke` compares that digest with the source's
current one, and on a mismatch raises `skill-changed` unless the caller passed `reload=True`.
OutRig never reloads a skill on its own.

`outrig.skills.reload(name)` removes exactly the package `outrig_skills.<normalized-name>` and its
dotted descendants -- the names that begin `outrig_skills.<normalized-name>.` -- from
`sys.modules`, then imports the skill again. A plain string prefix would be wrong: reloading
`review` would also remove `review_diff`.

What a reload leaves alone is documented rather than hidden. Functions, classes and
`@outrig.agent` declarations taken from the old module stay the old ones, and a running coroutine
keeps its frames. Objects built from the old module keep the old classes, so `isinstance` against
the reloaded class is false.

### Dependencies are checked, never installed

`dependencies` are compared with the distributions `importlib.metadata` finds when the skill loads,
and an unmet one fails the load with `unsatisfied-dependency`, naming each unmet specifier.
`requires-python` is compared with the interpreter's version and fails with `requires-python`.

Nothing is installed. Installing is a grant, and a file in a repository cannot make one. An agent
that wants the package runs `pip install` itself, which `discovery.md` allows and which then stands
in its history as code it chose to run. What an opt-in installer would need is
`plan/next/skill-dependency-installation.md`.

A met specifier does not prove the package loads. A package with compiled parts can match and
still fail at import, since this interpreter loads nothing compiled.

Failures to load carry distinct codes, because they need different responses:

- `unsatisfied-dependency` -- no distribution matches a declared requirement.
- `import-failed` -- the skill, or something it imports, raised while importing: a bug, a missing
  sibling, a bad relative import, a module that is not there. The original traceback goes with it.
- `native-extension-unavailable` -- only when the failure is verified to be compiled code this
  interpreter cannot load, on the evidence `0003-10`'s import diagnostic uses. When the cause is
  uncertain, the code is `import-failed`.

The other codes, for reference: `unknown-skill`, `metadata-invalid`, `duplicate-metadata-block`,
`requires-python`, `entry-not-found` for a module with no callable entry, `signature-unsupported`
for an entry whose signature cannot become `<Skill>Params` ("How `invoke` parses"),
`missing-binding`, `skill-changed`, and `skill-busy` if a reload during an invocation is refused
("Open questions").

## From `/name` to a call

```text
user types   /review-diff a.py b.py --max-parallel 2 --allow-partial
  -> the REPL finds review-diff in the catalog                      (no Python runs)
  -> the line reaches the main agent on its user channel, as text,
     with the resolved skill named on the Delivery
  -> the agent's code calls  await outrig.skills.invoke("review-diff", text)
  -> invoke loads the module, derives ReviewDiffParams from the entry's signature,
     has a child parse the text into one, injects bound parameters, and calls
     the entry
  -> the entry's return value or exception is the execution's result,
     in the agent's history and its round
```

**The REPL** takes a line beginning `/` as it does now: a built-in command runs, and otherwise the
name is looked up in the catalog. Built-in commands win -- `/quit`, `/help`, and `/approve` and
`/deny` from `boundary-policy.md` -- and a skill whose name one of them takes is reported when the
catalog is read; it stays reachable through `invoke`. A name that is neither is an unknown
command, as `run-new` reports it today.

**The line reaches the main agent as ordinary user-channel text.** For a skill with a `skill.py`,
the body is the line as typed; for an instruction-only skill, it is the `SKILL.md` body followed
by the line ("Instruction-only skills"). Either way the body is a `str`, so the channel's contract
holds, and the `Delivery` names the resolved skill in a new optional field (`messages.md`). The
field is the application's statement that the REPL resolved a directive, in the way `sender` is
its statement of who sent a message: a body that begins `/review-diff` is not a directive because
of its text. Only the main agent receives directives, since children have no user channel
(`work.md`).

A directive for a skill with a `skill.py` does not deliver its `SKILL.md` body, which would cost
context on every directive. The agent reads that body through `0003-27`'s accessor when the
parameter help is not enough.

**The agent calls `invoke`.** The preamble says what such a message asks, and the ordinary
response is short:

```python
d = await runtime.channels["user"].receive()        # d.skill == "review-diff"
await outrig.skills.invoke(d.skill, d.body.partition(" ")[2])
```

`invoke` returns a handle, as a typed call does (`typed-agents.md`, "Calling"): the invocation
starts when `invoke` is called, `await` gives the entry's return value or raises what it raised,
and an agent with something to do meanwhile keeps the handle, which `h.cancel()` ends -- the
parser child released, or the running entry interrupted. The field's name and `invoke`'s exact
signature are `0003-28`'s.

### How `invoke` parses

The text after the name is turned into arguments by a model, not by a parser, and the arguments
it produced are recorded where a reader finds them. `invoke(name, text, *, reload=False)` does
four things.

**It derives a parameter dataclass from the entry's signature.** The class is `<Skill>Params`,
the skill's name in CamelCase -- `review-diff` gives `ReviewDiffParams` -- declared
`@dataclass(frozen=True, kw_only=True)` as the result types in `typed-agents.md` are, so no
field's position forces a default onto another. Its fields come from `inspect.signature(entry)`:

- a parameter named in `bind` is not a field ("Bound parameters");
- every other positional or keyword parameter is a field of the same name, typed by its
  annotation and defaulted by its default -- a parameter without a default is a required field,
  and an unannotated parameter is a `str`;
- `*args: T` is a field named as the parameter, of type `list[T]`, defaulting to an empty list;
- `**kwargs` is refused, with `signature-unsupported`: there is no closed set of names to make
  fields from;
- the help in `[tool.outrig.skill.parameters]` becomes the field's `doc`, and so its
  `description` in the schema text (`typed-agents.md`, "`outrig.schema`").

Each field's type must be one `outrig.schema` takes: the serializable subset of `messages.md`,
plus `Literal` and `Annotated`. A parameter annotated with anything else fails the derivation
with `signature-unsupported`, naming the parameter, before any model is called -- so no
constructor is ever called on text a user typed. A default is the value `inspect.signature`
reports, evaluated once when the `def` ran, so a mutable default is shared between calls, as it
is in any Python.

**It has a child parse the text into an instance.** The parse is a typed agent call
(`typed-agents.md`), which the runtime declares for the skill:

```python
@outrig.agent
async def parse_params(text: str) -> ReviewDiffParams:
    """Turn `text`, what the user typed after `/review-diff`, into a ReviewDiffParams.

    <the rest of the runtime's instruction, then the entry's docstring>
    """
    ...
```

The text is the call's input, bound into the child's namespace as `text` and never interpolated
into the instructions. The instructions are the runtime's fixed text, whose wording is
`0003-28`'s, followed by the entry's docstring; the field types and the parameter help reach the
child as the result type's schema text, as they do for every typed call. The child completes with
an instance, decoded strictly: a key that matches no field, a value outside a `Literal`, or a
string where an `int` is declared is raised back to it with its path, and it completes again, up
to the attempt limit. It is a fresh child of the agent that called `invoke`, on that agent's
model -- the session's, when the main agent calls it -- and a child like any other: it counts
against `children-max`, its usage is attributed to it and added to the invocation and the round,
and it has no user channel, so it cannot ask what the text meant. It completes with its reading
of the text, or the call settles with `CompletionRejected`.

**It injects bound parameters and calls the entry.** The instance's fields and the bound objects
are passed as the signature asks: positional parameters positionally, in the signature's order,
with a bound one filled from the session's binding in its position; a `*args` field unpacked;
and keyword-only parameters by name. A `def` entry is called and an `async def` one awaited, and
the entry's return value is what awaiting the handle gives.

**Or it takes the instance from the caller.** `invoke(name, params=ReviewDiffParams(...))` skips
the model: an agent that already understood the text, or that asked the user and now knows,
constructs the instance and calls the entry through `invoke`, so bound parameters are still
injected and the invocation's events are still emitted. `params` is checked against the field
types as a typed call checks its inputs, and an instance of another skill's class is refused. The
class is reachable through `outrig.skills` (the accessor's spelling is `0003-28`'s), and `help()`
on it lists the fields with their types, defaults and help.

Either way, **the instance is on the invocation's `skill.invocation.started` event**, with whether
a child parsed it or the caller passed it ("Events"), so how the user's words became arguments is
in one place whichever route was taken.

Skill parameters and `invoke`'s own controls never share a namespace. A skill's parameters are
the instance's fields, and `invoke`'s controls, such as `reload`, are its Python keywords, so a
skill parameter named `reload` is a field like any other and reloads nothing.

**A worked example.** `review-diff`'s entry is

```python
async def main(repo, base: str = "origin/main", *paths: str,
               max_parallel: int = 4, allow_partial: bool = False) -> ReviewOutcome:
```

with `bind = { repo = "repo" }` and the `parameters` table above. The derived class is

```python
@dataclass(frozen=True, kw_only=True)
class ReviewDiffParams:
    base: Annotated[str, doc("The ref to diff against.")] = "origin/main"
    paths: Annotated[list[str], doc("Paths that limit the diff; none means every "
                                    "changed file.")] = field(default_factory=list)
    max_parallel: Annotated[int, doc("How many sections are reviewed at once.")] = 4
    allow_partial: Annotated[bool, doc("Return a partial outcome instead of raising "
                                       "when a section fails.")] = False
```

and `repo` is not in it.

- `/review-diff a.py b.py --max-parallel 2 --allow-partial` is parsed by the typed call into
  `ReviewDiffParams(base="origin/main", paths=["a.py", "b.py"], max_parallel=2,
  allow_partial=True)`, and the entry is called as
  `main(<repo>, "origin/main", "a.py", "b.py", max_parallel=2, allow_partial=True)`.
- `/review-diff just the auth changes` reaches the same parser, which completes with its reading
  of the words -- the paths it takes them to mean, or an empty `paths` -- and the instance is on
  the event. No rule turns four words into four paths because they are four words.
- A parse the child cannot complete -- a completion of the wrong shape three times, or rounds that
  end without one -- settles with `CompletionRejected`, which `invoke` raises to the agent, and
  nothing is called. The agent may ask the user, and then call `invoke` again with the text or
  with `params=`.

**Bound parameters.** A parameter `bind` names is not a field of `<Skill>Params`, so neither the
user nor the parser can set it, and it is filled from the session's binding of that name: the
object the runtime holds, not whatever the agent's variable of the same name now refers to. An
agent that has run `repo = None` still gets the binding. A `bind` entry naming a binding the
session lacks fails the call with `missing-binding`, and never creates one. A parameter is filled
because `bind` names it, never because a binding happens to share its name.

### Instruction-only skills

A skill with no `skill.py` follows the Agent Skills standard. `/name text` delivers a message whose
body is the `SKILL.md` body followed by the line as typed, with the skill named on the `Delivery`
as for any directive. The agent reads the message as it reads any other, and the body enters its
context then. `invoke` has no part in it.

## Authority

A skill runs with exactly the authority of the agent that invokes it, because it is that agent's
code running in that agent's execution. Loading one grants nothing. Its metadata cannot add tools,
mounts, network access, credentials or bindings the session was not configured with; it cannot
install anything; and it declares no code to run when the skill is listed or loaded. `bind` only
selects among bindings the session already has.

That is also why a repository's skills need no approval where its bindings do
(`hosted-objects.md`). A binding runs code on the host with the user's authority. A skill runs in
the container and does nothing the agent could not do by writing the same code; project skills
are repository content, trusted as much as any file in the workspace the agent can already run.

A hosted call a skill makes goes through the same boundary policy as any other
(`boundary-policy.md`). The invocation's context -- the skill's name, its digest, the invocation's
id -- is attached to those boundary events as diagnostic correlation, not as a principal. Code in
the same interpreter can set or clear such context, so policy does not treat it as identity.

## Events

The runtime emits an invocation's events, and the skill does not: the host when it delivers a
directive, and `invoke` for the rest.

- **`skill.directive.received`** -- the REPL resolved a `/name` and delivered it;
- **`skill.invocation.started`** -- the name, the source, the digest, the `<Skill>Params`
  instance as a bounded value preview, whether a child parsed it -- with that call's id -- or the
  caller passed it, and the names of the bindings injected;
- **`skill.invocation.finished`** -- returned, raised or cancelled, the duration, the result's
  type, and the model usage of the children the invocation ran, the parser included.

A parse the child could not complete ends the invocation before `skill.invocation.started`: the
parse call's own events carry the invocation's id and the rejection, and `invoke` raises
`CompletionRejected`.

They are not an audit of what the skill did locally. Its file writes, its subprocesses and direct
calls into its modules are not traced. Only its hosted calls are evented, per request, as every
hosted call is: each request's events are published on the session's stream
(`boundary-policy.md`), and the CLI writes them to `events.jsonl` only when `[events] mode =
"record"`. A direct helper call produces boundary events and no invocation events. The names are
candidates, and `0003-28` defines their schema in `observability.md`'s envelope.

The states one invocation passes through, as explanation rather than enum names: directive
received, delivered, loading, deriving, parsing, running, then returned, raised or cancelled -- or
rejected in parsing, when the typed call could not produce an instance.

The model usage of a child that a skill's code launches is attributed to the child and added to
its call, to the invocation, and to the main agent's round (`typed-agents.md`). The parser child's
usage is added the same way.

## Children see the same catalog

A child is a kernel in the same interpreter (`typed-agents.md`), with the same `sys.modules` and
the same finder, so its code can `invoke` a skill or import one of its modules exactly as the main
agent's code can. Its `invoke(name, text)` spawns a parser child of its own, under
`subagent-depth-max` as any call is, and `invoke(name, params=...)` spawns nothing. A directive
never reaches a child, since only the main agent has a user channel.

## Rejected alternatives

**Rejected: a second, OutRig-only root at `.agents/outrig/skills/`.** It would be a place no other
tool reads, which is its one advantage, and that advantage is not needed: other tools ignore a
`skill.py`. Two roots in one repository would also need a precedence rule between them, a question
one root does not have.

**Rejected: one file per skill, `<name>.py`, with its instructions in the module docstring.** It
is the smallest layout. It leaves no room for helper modules or long instructions, and no other
tool could read the instructions, since the tools that read skills look for a `SKILL.md`.

**Rejected: mounting skill directories into the container.** Simple for a project skill, which is
in the workspace already. It does not extend to the rest: an embedder's catalog would be mounted
into every container whether a skill was used or not, and user skills would need a mount of the
user's home.

**Rejected: running skills as subprocesses**, as the Agent Skills `scripts/` convention runs
programs. A subprocess cannot hold a hosted object, cannot await a typed child, and returns text.
A module's entry takes the session's bindings, awaits children, and returns Python values to the
code that called it.

**Rejected: `exec` of a skill into the agent's globals.** It puts the skill's names into the
agent's namespace, where they can replace the agent's own. A module has a namespace of its own,
relative imports, and an identity that `reload` can replace as a whole.

**Rejected: deterministic parsing by typer's rule**, the design until 2026-10-02. `text` was split
with `shlex.split` and parsed by a command built from the entry's signature on a vendored click:
a parameter with a default an `--option`, one without a positional argument, `*args` the
remaining positionals, a `bool` a `--flag/--no-flag` pair -- and when parsing failed, `invoke`
returned the usage and the agent wrote an explicit call. (typer itself was never the candidate:
it parses neither `*args` nor an `async def`, and it requires rich.) It was dropped for two
reasons. Exact directives are not supported out of the box -- mapping a user's words to a skill's
arguments is the model's job, and model-free dispatch is an alternative to evaluate, below. And
falling back to interpretation on a parse failure was unsound: a failure was the only signal that
the text was prose, and prose can parse as positional arguments -- `just the auth changes` parsed
as four paths and would have run a diff of them, with the explicit call never reached.

**Rejected: exact executable directives dispatched by the host**
(`potential/exact-skill-directives.md`). The host would parse a directive literally against the
entry's signature, validate it, schedule one invocation in the agent's kernel through the same
execution path, and return usage on an error, recording the origin as the user's directive. It is
a second entry path to execution, with scheduling of its own while a kernel is busy, and whether
exact, model-free dispatch is worth that is the evaluation that entry describes; it is not built
out of the box.

**Rejected: an `arguments = "text"` mode**, which would pass everything after the name to the
entry as one string for the skill to interpret. It was proposed for directives written as prose.
The typed parse reads prose and exact forms alike, and the instance it produces is on the
invocation's event; a string a skill interprets on its own is read where no event shows it.

**Rejected: the host calling the entry itself.** It would make a directive an execution path
outside the agent's history and round, which is what the first rule of this page excludes.

**Rejected: the host composing the call for the agent to submit.** An earlier proposal had the
REPL send a proposed call as source. `invoke(name, text)` puts the parse in one place, where the
signature is, and `invoke(name, params=...)` covers an agent that read the text itself.

**Rejected: re-mapping of arguments by the model without saying so.** Users will type
`/review-diff just the auth changes`. An interpretation has to be visible: the parser's instance
is on `skill.invocation.started`, and an agent that maps the text itself writes
`invoke(name, params=...)`, which stands in its history as the call that was made.

## Open questions

- How an embedder's sources rank against the project's and the user's. An ordered list in which
  the first source holding a name wins extends "the project wins"; `0003-27` decides.
- How an embedder's front end sends a directive. The skill is named on the message the host posts,
  so the session's user channel needs a way to name one (`embedding.md`, `0003-28`).
- How the preamble bounds a large catalog. The standard puts a skill's metadata at about 100
  tokens, so a hundred skills is about 10,000 tokens on every round. Listing a capped number with a
  count, and answering the rest from `outrig.skills`, would follow `discovery.md`'s inventory.
- Whether a child's instructions list the catalog. A child can use every skill, and the list costs
  tokens on every model call of every child.
- A reload while an invocation of the same skill runs in any kernel: refused with `skill-busy`, or
  made to wait.
- What the digest covers: every file of the skill, or only its Python. Data read through
  `importlib.resources` argues for every file.
- What `__file__` is on a skill module: unset, or a value that names the skill and is not a path.
- Whether metadata may narrow what a skill reaches. A narrowing would be presentation only, since
  a skill's code shares the interpreter with every binding (`agent-placement.md`).

## Unverified

- The Agent Skills frontmatter rules were read from agentskills.io's specification on 2026-09-30.
  Which tools read `.agents/skills` was read from their documentation in planning; none was run.
- The inline script metadata rules were read from PyPA's specification, and the `[tool.<name>]`
  rule from `pyproject.toml`'s. That nobody has registered `outrig` was checked on 2026-09-30, when
  PyPI's JSON endpoint for it answered 404.
- **The typed parse has not run.** That a model fills `ReviewDiffParams` from
  `a.py b.py --max-parallel 2 --allow-partial`, and from prose, is the design's expectation;
  `0003-28`'s acceptance runs the mechanism with a mock model, so how often a real model's reading
  matches what the user meant is unmeasured. The deterministic parser this replaced was checked
  under the payload's Python before it was dropped, and that check is what showed
  `just the auth changes` parsing as four paths. Nothing else on this page has been run.
- That `importlib.resources.files()` works for a package whose loader supplies a resource reader,
  and `inspect.getsource` through a loader's `get_source`, was read from the 3.13 documentation,
  not tried with a loader that fetches over the pipe.
- The cost of fetching over the pipe was not measured. Each module imported is a round trip to the
  host and through the source, so a skill of many small modules costs one round trip each.
