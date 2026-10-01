# 0003-28 -- `/name` reaches the main agent as a call it makes

## Context

`skills.md` settles that a slash directive asks the main agent to do something; it is not a way
around the agent. What runs is a call the agent makes, in its own execution, in its own history,
counted against its round. So `/review-diff a.py b.py --max-parallel 2` reaches the agent as text
on its user channel, with the resolved skill named on the `Delivery`, and the agent's code calls
`outrig.skills.invoke("review-diff", text)`.

Turning the text into arguments follows typer's rule, built with click alone: a parameter with a
default is an `--option`, one without is positional, `*args` takes the remaining positionals, and a
`bool` is a `--flag/--no-flag` pair. The research `skills.md` cites found typer itself unfit -- it
parses neither `*args` nor async functions, and it needs rich -- so click 8.5.0 is vendored,
through `0003-16`'s mechanism. When parsing fails, the agent gets the usage, and the call it then
writes itself is the explicit form, in its history. No model rewrites a user's arguments where
nobody can see it.

A parameter named in `bind` is not parsed from the text at all. It is filled from the session's
binding with the host's reference -- never from the agent's variable of the same name, which the
agent may have rebound.

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
  when the usage is not enough. For an instruction-only skill, the body is its `SKILL.md` body
  followed by the line as typed, read through the skill's source, with the skill named the same
  way. The embedding API gains a way to send a message naming a skill, so an embedder's front end
  can send a directive.
- **The preamble says what such a message asks**: for a skill with a `skill.py`, call
  `outrig.skills.invoke(name, text)` with the text after the name, as fork 4 qualifies.
- **`outrig.skills.invoke(name, text, *, reload=False)`** imports the skill, tokenizes `text` with
  `shlex.split`, and builds a click command from the entry's signature by typer's rule; an option's
  flag is the parameter's name with hyphens, so `max_parallel` is `--max-parallel`. Parameters
  named in `bind` are left out of the command and injected from the session's bindings. The
  parameters before `*args` are passed positionally, with defaults filled from the signature, and
  keyword-only ones by name. A `def` entry is called, an `async def` one awaited, and the entry's
  return value is `invoke`'s.
- **When the text does not parse, nothing is called**, and `invoke` returns the usage -- click's
  message for what failed and the command's usage line -- as a value of its own type, so it cannot
  be mistaken for what an entry returned. `--help` returns the usage the same way, catching the
  `Exit` click raises after printing it. The explicit call form -- Python values rather than text,
  with bound parameters still injected and the same events -- is how the agent then calls the
  skill; its spelling per fork 2.
- **Vendored click 8.5.0**, added by `build.rs` through `0003-16`'s vendoring mechanism with its
  own hash pin and cache key, into the directory of vendored packages mounted read-only beside the
  payload, where RPyC is. It is not under `outrig`, which holds no vendored code (`0003-24`). The
  name it is imported by, per fork 3.
- **Failures before the call**: `missing-binding` for a `bind` entry naming a binding the session
  lacks, `entry-not-found` for an `entry` the module does not define -- which discovery cannot check
  without running Python -- and `unknown-skill`. Each is raised before anything is called.
- **`skill-changed` at `invoke`**, checked against `0003-27`'s digest. `reload=True` reloads first,
  as `outrig.skills.reload` does; it is a Python keyword of `invoke`, never parsed from the text.
  A reload while an invocation of the same skill runs, per fork 5.
- **Invocation events**, emitted by the runtime rather than by the skill -- the host when it
  delivers a directive, `invoke` for the rest -- in `0003-13`'s envelope: directive received, when
  the REPL resolves one; and, sharing the invocation's id, invocation started, with the skill, its
  source, its digest, a bounded preview of the arguments and the names of the bindings injected,
  and invocation finished, with whether the entry returned, raised or was cancelled -- or that
  usage was returned -- the duration, the result's type, and the model usage of the typed children
  it ran. The invocation is the link between a call and the main agent's round that `0003-25`'s
  spend chain leaves for this task: a child's usage is added to the invocation it ran under, and
  from there to the round (`typed-agents.md`).
- **Correlation.** While an invocation runs, the boundary events of the hosted calls made in it
  and the start events of the typed-agent calls it makes carry the invocation's id, through a
  context variable `invoke` sets around the entry, which tasks the entry starts and `to_thread`
  workers inherit. It is diagnostic context, not a principal: policy never reads it, and code in
  the same interpreter can set or clear it (`skills.md`). A skill's helper called directly -- its
  module imported and the function called, without `invoke` -- produces the boundary events of
  its hosted calls without the id, and no invocation events.

## Acceptance

Against a skill named `review-diff` whose entry is the one `skills.md` uses, with `bind` mapping
`repo` to the binding `repo`:

```python
async def main(repo, base: str = "origin/main", *paths: str,
               max_parallel: int = 4, allow_partial: bool = False): ...
```

- `/review-diff a.py b.py --max-parallel 2 --allow-partial` binds `paths=("a.py", "b.py")`,
  keeps `base`'s default, and sets `max_parallel=2` and `allow_partial=True`.
- `/review-diff --base origin/x a.py` sets `base="origin/x"` and `paths=("a.py",)`.
- `--allow-partial` and `--no-allow-partial` each set the flag they name.
- **Quoting and `--` follow POSIX rules.** `--base "origin/release 2.1" "my file.py"` sets
  `base="origin/release 2.1"` and `paths=("my file.py",)`, and with `a.py -- --odd.py` the token
  after `--` is a path: `paths=("a.py", "--odd.py")`.
- **A bad option returns usage and calls nothing**, for `--max-parallel two` and for `--nope`, and
  the agent's explicit call that follows appears in `runtime.history` as an ordinary execution.
- **A user cannot set a bound parameter**: `/review-diff --repo other a.py` returns usage, and the
  entry is not called.
- `--help` returns the usage, and nothing is called.
- **Injection uses the binding after the agent sets `repo = None`**: the entry receives the hosted
  object, not `None`.
- **A skill parameter named `reload` parses normally**: `--reload` sets the skill's parameter and
  reloads nothing.
- A `bind` naming a binding the session lacks fails with `missing-binding`, and the entry is not
  called.
- An unclosed quote, as in `/review-diff don't`, returns usage rather than raising.
- **A `Literal` is a choice, and surplus positionals are refused.** With a skill `tag` whose entry
  is `def main(name: str, kind: Literal["light", "annotated"] = "light")`, `/tag v1 --kind heavy`
  returns usage naming the choices, and `/tag v1 v2` returns usage, since the entry has no
  `*args`; neither calls the entry.
- **An annotation outside the closed set never runs** (with fork 1's recommendation): a skill whose
  entry has a parameter annotated with a class whose constructor writes a file fails `invoke` when
  the command is built, naming the parameter, and no file is written.
- **Failures before the call carry their codes**: an `entry` the module does not define fails with
  `entry-not-found`, and a name no source holds with `unknown-skill`; neither calls anything.
- **A reload during an invocation is refused** (with fork 5's recommendation):
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
- **Invocation events**: a directive the REPL resolves publishes directive received, and the
  agent's `invoke` publishes invocation started -- the skill, its source, its digest, the argument
  preview and the injected binding names -- and invocation finished, sharing the invocation's id:
  returned, with the duration and the result's type; raised, for an entry that raises; and usage
  returned, for text that does not parse.
- **Spend reaches the invocation**: the finished event of an invocation whose entry ran typed
  children carries the sum of their usage, and the main round's total includes it once.
- **Calls made in an invocation carry its id.** A hosted call the entry makes, one made from a
  `to_thread` worker it starts, and an `@outrig.agent` call it makes carry the invocation's id in
  their events. The same helper called directly, after `import outrig_skills.review_diff.skill`,
  produces the hosted call's boundary events without an invocation id, and no invocation events.
- **The parts compose, end to end**, through `run-new` with a mock model, a mock evaluator and a
  binding of `0003-16`'s fixture library. A `/name` directive runs a skill whose entry uses its
  bound binding; makes three `@outrig.agent` calls, which return handles that `asyncio.gather`
  accepts, one of whose children completes with a wrong shape once and is repaired; and makes a
  hosted call that an `evaluate` rule sends to the evaluator, which answers `escalate`, and that
  runs once `/approve` is typed.
  The events' parentage matches the run: each child call under the invocation, the repair under
  its call, and each boundary event under its execution, the entry's carrying the invocation's
  id. Each child's usage is in its own events and in its call's, the invocation's and the round's
  totals, once each, and the evaluator's usage is in its own decision event and in none of those
  totals.
- **Shutdown with typed children running**: the same run, shut down while the three calls are
  under way and the escalated call waits for an answer, settles each call's waiter with the
  documented error, cancels the pending escalation, lists every child's work and every hosted
  call in the `ShutdownReport`, and returns within the test's deadline.
- `crates/outrig/public-api.txt` regenerated: the way to send a message naming a skill, the
  invocation events, and the invocation's id on the events that carry it are its only additions.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Which annotations convert -- Recommended: a closed set**: `str`, `int`, `float`, `bool`,
   `Literal` of strings as a choice, and `Optional` of those, with an unannotated parameter taken
   as `str`. Any other annotation on a parsed parameter is an error when the command is built,
   rather than a constructor called on text a user typed.
2. **The explicit call form -- Recommended: a form of `invoke` that takes the skill's arguments as
   a tuple and a dict**, apart from `invoke`'s own keywords, so a control keyword such as `reload`
   never shares a namespace with a skill parameter -- the same reason a skill's `--reload` in the
   text parses as the skill's own.
3. **How agent code's own click is kept from shadowing the vendored copy -- Recommended: the
   vendored copy under a private top-level name, such as `_outrig_click`, with click's absolute
   self-imports left unreachable.** Neither copy then shadows the other: an agent's own
   `pip install click` neither replaces OutRig's copy nor is replaced by it. click 8.5.0 imports
   `click.shell_completion` by its absolute name in six places, all inside `shell_complete` methods
   in `core.py` and `types.py`. Under a private name those imports would reach the agent's click,
   or fail, if anything called them; `invoke` never completes a shell line and its command object
   never leaves it, so nothing does. Rewriting the six imports when `build.rs` unpacks the wheel is
   the alternative if a path to them turns up. A top-level `click`, before or after the agent's
   packages on `sys.path`, makes one copy shadow the other: OutRig's parser runs on whatever click
   the agent installed, or the agent's packages import OutRig's version.
4. **Text that parses but was meant as prose -- Recommended: the preamble tells the agent to read
   the text first.** With `*paths`, `/review-diff just the auth changes` parses as four paths. The
   preamble says to pass the text to `invoke` when it reads as arguments, and otherwise to write
   the explicit call, after asking the user if the meaning is unclear. A check inside `invoke`
   cannot tell four words from four paths.
5. **A reload while an invocation of the same skill runs -- Recommended: refused with
   `skill-busy`.** Waiting instead would wait forever when the reload is asked for from inside
   that invocation, since the invocation cannot end while it waits. The invocation already running
   keeps the code it started with either way.

## Dependencies

- **Hard: `0003-21`.** Injection passes a binding's host reference, which needs bindings in the
  kernel. Through it, `0003-16`, whose vendoring mechanism adds click.
- **Hard: `0003-22`.** `/approve` and `/deny` are built-ins a skill's name can collide with, and
  the end-to-end acceptance meets an escalated call.
- **Hard: `0003-23`.** The end-to-end acceptance's escalation comes from the evaluator, whose
  usage it checks is kept apart.
- **Hard: `0003-26`.** The end-to-end acceptance runs typed children through `@outrig.agent`;
  through it, `0003-25`'s spend chain, which the invocation joins.
- **Hard: `0003-27`.** The catalog the REPL matches against, the loader, the digest and
  `outrig.skills.read` are its.

## See also

- `plan/phase/0003-python/skills.md` -- the directive, click parsing by typer's rule, bind
  injection, and the open questions this task's forks answer.
- `plan/phase/0003-python/messages.md` -- the `Delivery` this adds a field to.
- `crates/outrig-cli/src/cli/run_new/converse.rs` and `crates/outrig-cli/src/repl.rs` -- where
  commands are matched and help is composed.
- `plan/next/children-have-a-user-channel.md` -- a child-addressing REPL form must not collide with
  `/name`.
