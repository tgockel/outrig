# 0003-47 -- Instructions are composed from contributions

## Context

A child's instructions are composed once, by `child_preamble`
(`crates/outrig/src/agent/orientation.rs:168-253`): how the agent acts (`ACT`, `:45-47`), that it
is a child no user reaches (`:176-181`), the prompt its parent wrote -- the class docstring for an
instance -- the inputs by name, type and preview (`:184-194`), a brief per kind that for a request
child lists each channel with its docstring and the schema text of its request and reply types and
how to answer (`:213-241`), then what every agent is told last (`rest`, `:111-142`), which lists
the session's bindings (`hosted`, `:267-295`). The preamble is given to the model once
(`crates/outrig/src/agent/build.rs:95`) and recorded once as `model.instructions`
(`crates/outrig/src/agent/mod.rs:334`); only a round's opening line changes, and an instance's
first is `ORIENTING` (`orientation.rs:360-362`). Nothing is read off a Python object to compose
it: docstrings, schema text and input previews (`_agents.py:186-195`), no `repr` and no
`__dict__`. The repository's instructions are capped at 32 KiB
(`crates/outrig/src/agent/instructions.rs:20`) and the skills list at 50 entries or 12 KiB
(`orientation.rs:298-301`). A spawn line is bounded
at `MESSAGE_MAX`, 1 MiB (`_refuse_long`, `crates/outrig/src/python/interpreter.py:4259-4266`).
`0003-42` gives the orientation a facility manifest in place of the bindings list, `0003-45` a
sentence about `self`, and `0003-46` a root whose construction answers on a line of its own.

OR-AG7 asks that, after full initialization, OutRig compose execution and channel rules, the
class's and the application's task contributions, and the actual facility and channel manifest;
that the application supply content and never assemble the prompt; that omitted child
facilities are not advertised and object fields are not dumped; and that the contribution
signature and the opening-round timing stay open. The validation placed the hook as a
kernel-to-host contribution step on the launch path, composed into the orientation Rust builds,
and for the owner-constructed root a protocol line of its own; the conflict check found no
contradiction, since the design already fixes instructions at spawn and interpolates nothing
into a docstring. `task-agents.md`, "Instructions are composed from contributions", records the
order and the rules.

## Goal

A task agent's instructions are composed by OutRig from its rules, the class's docstring, the
application's contributions, the facility manifest and the channel declarations -- each a
bounded section, none interpolated into another -- with the contributions collected by a hook
OutRig calls once, after attachment.

## Deliverables

- **The hook.** An optional method of the class (fork 1 names it) with signature `(self) ->
  Iterable[outrig.Section]`, where `outrig.Section(title, text)` is a frozen dataclass of two
  `str`. OutRig calls it once per instance, after attachment, in the kernel that constructed the
  instance -- the constructing kernel for an in-VM child, inside the launch task before the spawn
  line is sent; the root's own kernel for an owner-constructed root, before its answer -- and on
  the host for `0003-35`'s host-declared class, whose sections cross with its declaration. It may
  read `self`; `help(outrig.Agent)` says it must call no model and should make no hosted call,
  which is documented and not policed. A return that is not an iterable of `Section` raises
  `TypeError` at construction; a hook that raises fails the construction as `0003-41` has it.
- **Bounds.** A title at most 64 characters; a section's text at most 16 KiB; every section
  together at most 32 KiB, the repository instructions' cap. Past a bound, construction fails
  with `ValueError` naming the section and its size (fork 2), before any round.
- **What crosses.** The sections cross interpreter to host as `contributions: [{title, text}]`
  on the spawn line, or on the root's construct answer (`0003-46`); the host composes and never
  interpolates.
- **The composition.** `child_preamble` takes the sections, the facility manifest (`0003-42`)
  and the `self` sentence (`0003-45`), and composes in `task-agents.md`'s order: `ACT`; the child
  line, or for a root "the program that owns this session constructed you; no user reaches you";
  the docstring, unformatted; each section under its title, verbatim, in order; the inputs; the
  `self` sentence, naming an alias when the record bound one (`0003-45`); the facility manifest,
  each entry by name and kind with the text its kind allows, and nothing read from any object
  beyond what `0003-42` reads; the channels and how to answer; for an owner-constructed root, the
  skill catalog as the primary's preamble carries it under `0003-32`'s bound, and never for a
  child; `rest()`, with the hosted-call rule when a facility is a `binding`. `0003-37`'s sentences
  in the request child's preamble stay.
- **The opening's order** (OR-A1-5, OR-A7-3). The instructions above are the system prompt and
  carry no delivery's body. The orienting round opens on its fixed lead and, when a delivery
  already waits on an external channel the agent holds, its opening line announces what waits by
  channel and count, after the instructions and never as a body in them, as
  `external-channels.md`, "Initial input is a delivery", documents and `0003-51` records; the
  delivery is read by code; empty initial input delivers nothing and the opening announces
  nothing; the round requires no request or delivery to exist. `task-agents.md`, "Instructions
  are composed from contributions", states the same order, and the orientation says in one
  sentence that a delivery is announced in a round's opening line and read by code.
- **The root's lead** per fork 3.
- **Events and the close table.** `agent.instance.started` gains `contributions`, each
  `{title, length}`; `model.instructions` already carries the composed text;
  `doc/reference/events.md` says so. No close row: a hook runs inside construction, which
  `0003-41` and `0003-46` cover; the
  task's `## Decisions` says so.
- **`## Decisions` records**: the hook's name and signature; where it runs for each construction
  site; the bounds and the refusal; that composition happens once, before the orienting round,
  and why; the root's lead; and that the application supplies content and OutRig the assembly.

## Acceptance

Each is a mock-driven test.

- A class whose hook returns `[Section("Task", f"Review {self._task.title}")]` after `__init__`
  set `_task`: the child's `model.instructions` holds, in order, `ACT`, the child line, the
  docstring unchanged with its braces, a section titled `Task` with that text, the `self`
  sentence, the facility manifest listing the dict's entries and no other binding, the channels
  with schema text, and `rest()`. An attribute `self.secret` set in `__init__` appears nowhere
  in it.
- Without the hook, the instructions are `0003-34`'s plus the `self` sentence and the manifest;
  `0003-34`'s composition test is extended, not replaced.
- With empty initial input, the root's first round carries the instructions and an opening line
  with the lead and no announcement, and nothing invented; with a request already waiting on a
  declared channel when the root launches (a delivery on an external channel once `0003-51`
  lands), the opening line announces it by channel and count after the lead, and no body of it
  appears in `model.instructions`.
- A section's text holding `{}` and `{x}` appears verbatim.
- A section of 16 KiB plus one byte, and three sections totaling 32 KiB plus one byte, each fail
  construction naming the section; no spawn, no model call. A title of 65 characters likewise.
- A hook that raises: the constructor call raises; no spawn.
- A hook returning a `str`, or a list holding a tuple: `TypeError` at construction.
- The hook runs once: a counter on the instance reads 1 after three rounds, and
  `model.instructions` is recorded once.
- An `object` facility whose `__repr__` writes a file: the file does not exist after the
  instructions are composed.
- The owner-constructed root (`0003-46` landed): sections on the construct answer appear in the
  root's `model.instructions`, and its first lines say the owner's program constructed it.
- `0003-35`'s host-declared class with the hook: its sections cross with the declaration and
  appear in the child's instructions.
- `agent.instance.started.contributions` lists each title and length.
- `crates/outrig/public-api.txt` regenerated; `cargo test --workspace`,
  `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **The hook's spelling -- Open; recommended: `__outrig_instructions__`.** A dunder OutRig
   owns collides with no request method and no helper. Alternatives: `instructions` or
   `contribute`, each a name every subclass would then be unable to use for a channel.
2. **Past a bound -- Recommended: refuse at construction.** The alternative cuts the section with
   a note, as the skills list is cut at 12 KiB, which changes the instructions silently for the
   model where a refusal tells the author before any round.
3. **The root's opening lead -- Recommended: `ORIENTING`'s text with one clause more for a root
   whose instructions carry contributions: that its task is in them and this round is where it
   begins.** The alternative leaves `ORIENTING` unchanged, saying only that the round is for
   getting ready. Not a fork: where the first delivery's announcement goes -- in the opening line,
   after the lead, never in the instructions -- which `external-channels.md`, "Initial input is a
   delivery", documents and this task states.

Composition once, before the orienting round, is not a fork: the preamble is given to the model
once, and `task-agents.md` records the per-round alternative as rejected.

## Dependencies

- **Hard: `0003-45`.** The `self` sentence, and an instance that is in the child.
- **Hard: `0003-42`.** The facility manifest this composes.
- **Hard: `0003-41`.** The hook runs in the construction sequence, after attachment.
- **Soft: `0003-46`.** The root's line and lead; the sections on its answer.
- **Soft: `0003-35`.** The hook on the host for a host-declared class.
- **Soft: `0003-37`.** The request child's preamble sentences, unchanged here.

## See also

- `plan/phase/0003-python/task-agents.md` -- "Instructions are composed from
  contributions", "Rejected alternatives".
- `plan/phase/0003-python/facilities.md` -- "The view is the agent's, the grant is the
  session's".
- `plan/phase/0003-python/agent-classes.md` -- "The declaration", "Construction and readiness".
- `plan/phase/0003-python/typed-agents.md` -- "Inputs are values".
- `plan/phase/0003-python/discovery.md` -- "What goes in the preamble".
- `crates/outrig/src/agent/orientation.rs`, `children.rs` (`make`), `instructions.rs`,
  `crates/outrig/src/python/interpreter.py` (`_Instance.launch`, `_spawn`).
