# Skills

A **skill** is reusable know-how that a repository, a user, or an embedding application supplies to
an agent: instructions in a `SKILL.md`, and optionally a Python module the agent imports and
calls. This page settles where skills come from, how they are listed without running any of their
code, how one loads as a module, and how a line like `/review-diff a.py b.py` becomes a call the
main agent makes. The design was settled in planning on 2026-09-30. `0003-27` builds the sources,
discovery, the loader and the preamble; `0003-28` builds the directive, argument parsing and
injection.

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
# ///
```

- `entry` names the entry function, `main` by default. It may be a `def` or an `async def`.
- `bind` maps an entry parameter to a binding the session has (`hosted-objects.md`): here,
  parameter `repo` receives binding `repo`. See "Bound parameters" below.

An unknown key in `[tool.outrig.skill]` is reported as a warning rather than ignored.
`requires-python` and `dependencies` are the specification's own keys, checked when the skill
loads.

`[tool]` has `pyproject.toml`'s semantics, under which `[tool.<name>]` belongs to whoever owns
`<name>` on PyPI. Nobody has registered `outrig` there, so OutRig's claim to `[tool.outrig]` is
nominal; registering it is a separate decision for the maintainer.

Discovery does not read the entry's signature, since that means parsing Python and the host does
not. The parameters become known when the module is imported, which is when `invoke` builds its
parser.

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
accessor is `0003-27`'s). An executable skill's parameters are learned from `invoke`'s usage, and
from `help()` on its module once loaded.

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
  tracebacks show a skill's lines, `inspect.getsource` works on its functions -- `0003-26`'s body
  check needs that for an `@outrig.agent` declared in a skill -- and `importlib.resources` reads
  its data.
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
  is ordinary Python: it skips `invoke`'s parsing and invocation events, and does not skip the
  boundary policy a hosted call goes through.

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
`requires-python`, `entry-not-found` for a module with no callable entry, `missing-binding`,
`skill-changed`, and `skill-busy` if a reload during an invocation is refused ("Open questions").

## From `/name` to a call

```text
user types   /review-diff a.py b.py --max-parallel 2 --allow-partial
  -> the REPL finds review-diff in the catalog                      (no Python runs)
  -> the line reaches the main agent on its user channel, as text,
     with the resolved skill named on the Delivery
  -> the agent's code calls  await outrig.skills.invoke("review-diff", text)
  -> invoke loads the module, builds a parser from the entry's signature,
     parses the text, injects bound parameters, and calls the entry
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
context on every directive. The agent reads that body through `0003-27`'s accessor when
`invoke`'s usage is not enough.

**The agent calls `invoke`.** The preamble says what such a message asks, and the ordinary
response is short:

```python
d = await runtime.channels["user"].receive()        # d.skill == "review-diff"
await outrig.skills.invoke(d.skill, d.body.partition(" ")[2])
```

The field's name and `invoke`'s exact signature are `0003-28`'s.

### How `invoke` parses

`invoke(name, text, *, reload=False)` splits `text` with `shlex.split` -- POSIX quoting, with no
expansion, globbing or substitution -- and parses the tokens with a command built from the entry's
signature by **typer's rule**, on a vendored click 8.5.0:

- a parameter with a default is an option, `--name`, its underscores written as hyphens;
- a parameter without a default is a positional argument, and `*args` is a positional argument
  that takes any number;
- a `bool` parameter is a pair of flags, `--name/--no-name`;
- the parameters before `*args` are passed positionally, each one the text did not set with its
  default filled in, and keyword-only parameters are passed by name.

A default filled in is the value `inspect.signature` reports, evaluated once when the `def` ran.
Nothing evaluates it again or takes one from metadata, so a mutable default is shared between
calls, as it is in any Python.

Conversion follows the annotation, over a closed set of types: `str`, `int`, `float` and `bool` at
least. Any other annotation is an error when the parser is built, and never a constructor called
on the user's text. The exact set, and what an unannotated parameter is, are `0003-28`'s.

Skill parameters and `invoke`'s own controls never share a namespace. A skill's parameters travel
inside `text`, and `invoke`'s controls, such as `reload`, are its Python keywords, so a skill
parameter named `reload` is parsed like any other and reloads nothing.

**A worked example.** `review-diff`'s entry is

```python
async def main(repo, base: str = "origin/main", *paths: str,
               max_parallel: int = 4, allow_partial: bool = False) -> ReviewOutcome:
```

with `bind = { repo = "repo" }`. The command has `[PATHS]...` and the options `--base`,
`--max-parallel` and `--allow-partial/--no-allow-partial`; `repo` is bound, so the parser does
not have it.

- `a.py b.py --max-parallel 2 --allow-partial` calls
  `main(<repo>, "origin/main", "a.py", "b.py", max_parallel=2, allow_partial=True)`. `paths` is
  `("a.py", "b.py")`, and `base`, which must be passed positionally because `*paths` follows it,
  keeps its default.
- `--base origin/x a.py` calls
  `main(<repo>, "origin/x", "a.py", max_parallel=4, allow_partial=False)`.
- `--max-parallel two` calls nothing, and returns the usage: `'two' is not a valid integer`.

**Bound parameters.** A parameter `bind` names is left out of the parser, so the user cannot set
it, and is filled from the session's binding of that name: the object the runtime holds, not
whatever the agent's variable of the same name now refers to. An agent that has run `repo = None`
still gets the binding. A `bind` entry naming a binding the session lacks fails the call with
`missing-binding`, and never creates one. A parameter is filled because `bind` names it, never
because a binding happens to share its name.

**When the text does not parse**, `invoke` calls nothing and returns the usage -- click's message
for what failed, and the command's usage line -- as a value of its own type, so that it cannot be
mistaken for what an entry returned. The agent then turns the user's text into an **explicit
call**: a form of `invoke` that takes Python values for the entry's parameters instead of text,
still injects bound parameters, and still emits the invocation's events (its spelling is
`0003-28`'s). The explicit call is code the agent submitted, so it stands in the history, where
anyone reading it sees how the user's words became arguments. The agent may ask the user first.
What it may not do is re-map the arguments without saying so, and the explicit call is how it
says so. An unbalanced quote ends the same way: `shlex.split` raises on the apostrophe in
`don't`.

The explicit call is reached only when parsing fails. Text that parses but was meant as prose
never reaches it: `just the auth changes` becomes four `paths`. What to do about that is an open
question, and `0003-28` owns it ("Open questions").

**Why click and not typer.** typer's rule is the part worth having -- it is how a Python
programmer already expects a signature to become a command line -- and typer itself does not fit.
It cannot parse `*args` or call an `async def`; it requires rich, whose terminal formatting has no
terminal to write to here; and parsing without calling the function is not a path it supports,
where `invoke` must parse, inject the bindings, and then make the call itself. click 8.5.0 alone
is pure Python, and a small builder applies typer's rule over it. It is vendored by `0003-16`'s
mechanism into the directory mounted read-only beside the payload. How an agent's own install of
a package named `click` is kept from shadowing that copy is `0003-28`'s fork: click imports itself
by absolute name, which breaks under a private name.

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

- **directive received** -- the REPL resolved a `/name` and delivered it;
- **invocation started** -- the name, the source, the digest, a bounded preview of the arguments,
  and the names of the bindings injected;
- **invocation finished** -- returned, raised or cancelled, the duration, and the result's type.

They are not an audit of what the skill did locally. Its file writes, its subprocesses and direct
calls into its modules are not traced. Only its hosted calls are evented one request at a time, as
every hosted call is: each request's events are published on the session's stream
(`boundary-policy.md`), and the CLI writes them to `events.jsonl` only when `[events] mode =
"record"`. A direct helper call produces boundary events and no invocation events. The names are
candidates, and `0003-28` defines their schema in `observability.md`'s envelope.

The states one invocation passes through, as explanation rather than enum names: directive
received, delivered, loading, parsing, running, then returned, raised or cancelled -- or usage
returned, when the text did not parse.

The model usage of a child that a skill's code launches is attributed to the child and added to
its call, to the invocation, and to the main agent's round (`typed-agents.md`).

## Children see the same catalog

A child is a kernel in the same interpreter (`typed-agents.md`), with the same `sys.modules` and
the same finder, so its code can `invoke` a skill or import one of its modules exactly as the main
agent's code can. A directive never reaches a child, since only the main agent has a user channel.

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

**Rejected: typer**, for the reasons in "Why click and not typer".

**Rejected: an `arguments = "text"` mode**, which would pass everything after the name to the
entry as one string for the skill to interpret. It was proposed for directives written as prose.
Parsing by typer's rule replaced it, with the agent's explicit call when parsing fails: arguments
are parsed in one place, and an interpretation of the user's words is a call the agent writes,
where the history shows it. Prose that happens to parse as arguments is still open ("Open
questions").

**Rejected: the host calling the entry itself.** It would make a directive an execution path
outside the agent's history and round, which is what the first rule of this page excludes.

**Rejected: the host composing the call for the agent to submit.** An earlier proposal had the
REPL send a proposed call as source. `invoke(name, text)` puts parsing in one place, the
interpreter, where the signature is, and the explicit call covers what the parser cannot.

**Rejected: re-mapping of arguments by the model without saying so.** Users will type
`/review-diff just the auth changes`. An interpreted call has to appear in the history as the call
that was made.

## Open questions

- How an embedder's sources rank against the project's and the user's. An ordered list in which
  the first source holding a name wins extends "the project wins"; `0003-27` decides.
- How an embedder's front end sends a directive. The skill is named on the message the host posts,
  so the session's user channel needs a way to name one (`embedding.md`, `0003-28`).
- **Text that parses is not proof the user meant arguments.** With `*paths`, `/review-diff just the
  auth changes` parses as four paths and runs a diff of them. The explicit call is reached only on
  a parse failure, so either the preamble tells the agent to read the text before passing it to
  `invoke`, or `invoke` needs a way to decline prose; `0003-28` decides.
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
- **The parsing claims were checked** with click 8.5.0's wheel under the payload's Python, through
  a forty-line builder written for the check rather than OutRig's: the worked example's three
  cases, `just the auth changes` parsing as four paths, an unknown option and a bad integer raising
  `UsageError`, `don't` failing in `shlex.split`, and `--help` printing the usage and raising
  click's `Exit`, which `invoke` has to catch. Nothing else on this page has been run.
- typer's limits were read from typer 0.27.2's package metadata, which requires rich
  unconditionally, and from its release notes, which add support for neither `*args` nor `async`.
  typer itself was not run.
- That `importlib.resources.files()` works for a package whose loader supplies a resource reader,
  and `inspect.getsource` through a loader's `get_source`, was read from the 3.13 documentation,
  not tried with a loader that fetches over the pipe.
- The cost of fetching over the pipe was not measured. Each module imported is a round trip to the
  host and through the source, so a skill of many small modules costs one round trip each.
