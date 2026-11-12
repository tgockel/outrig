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
at `MESSAGE_MAX`, 1 MiB (`_refuse_long`,
`crates/outrig/src/python/outrig/_interpreter.py:4259-4266`). `0003-42` gives the orientation a
facility manifest in place of the bindings list, `0003-45` a sentence about `self`, and `0003-46` a
root whose construction answers on a line of its own.

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
  Iterable[outrig.Section]`, where `outrig.Section(title, text)` is a frozen dataclass of two `str`.
  OutRig calls it once per instance, in the constructor call, after attachment's checks and before
  the launch is scheduled -- in the constructing kernel for an in-VM child; in the root's own kernel
  for an owner-constructed root, before its answer -- and in the program for `0003-35`'s
  host-declared class, whose sections cross with its declaration. It may read `self`;
  `help(outrig.Agent)` says it must call no model and should make no hosted call, which is
  documented and not policed. A return that is not an iterable of `Section` raises `TypeError` at
  construction; a hook that raises fails the construction as `0003-41` has it.
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
  `crates/outrig/src/python/outrig/_interpreter.py` (`_Instance.launch`, `_spawn`).

## Decisions

- **The hook is `__outrig_instructions__(self)`, which returns an iterable of `outrig.Section`.**
  Fork 1 as recommended. It is a dunder OutRig owns, as `__outrig_attach__` is, and `Agent` defines
  it as a default that returns `()`, so OutRig calls `cls.__outrig_instructions__(agent)`
  unconditionally, `help(outrig.Agent)` lists it, and `_declare_class` already refuses a channel of
  that name. The default has no docstring, since `help(outrig.Agent)` has no room for one; the class
  docstring says what the hook is. `outrig.Section(title, text)` is a frozen dataclass of two `str`,
  exported by `outrig` and listed on `doc/reference/python.md`. It checks nothing itself: the one
  validator, `_agents.contributions`, sees every section the hook returns, wherever the hook runs.
- **Where it runs.** The task said the hook runs "inside the launch task before the spawn line is
  sent" and also that "the constructor call raises; no spawn". A raise inside `_Instance.launch`
  is caught and reaches `ready()`, so only a call made synchronously meets the acceptance, which
  six lines state; the two lines that said otherwise -- this task's first Deliverable and
  `task-agents.md`, "The shape", step 4 -- are corrected. The hook runs as the last step of
  `_Attached.attaching`, which `_Instance` and `_Adopted` share: after `__init__`,
  `__outrig_attach__` and attachment's checks, with the mapping classified, and before the
  instance's record is made or its launch scheduled. For a host-declared class it runs in
  `_Root.__init__`, before its line is built. A raise leaves nothing registered, so nothing is
  retired; for an owner's root it is the construction's failure, `agent.construct.failed` with
  `raised`. `agent._outrig` is still `None` there, so a request method called in the hook raises
  `TypeError`, which `help(outrig.Agent)` says.
- **Bounds and the refusal (fork 2, refused).** A title is 1 to 64 characters, a text at most 16
  KiB, and every title and text together at most 32 KiB, `instructions.rs`'s cap. All are measured
  in UTF-8 as sent: a lone surrogate counts as the escape `_clean` writes. The total is added as
  each section is read, so a hook that never stops is refused at the bound. `ValueError` names the
  section and its size; `TypeError` is a return that is not an iterable of `Section` (a `str`
  included; `iter()` decides, so a class that defines only `__getitem__` is one) or a field that is
  not a `str`. An error names the section by its number and its title. An empty title is refused,
  since a section is composed under its title. The host re-checks nothing: the line comes from
  OutRig's own interpreter, and a spawn's `prompt`, which agent code writes, is bounded by the line
  alone. The event's `length` is the text's bytes, the unit of the section bound.
- **The composition, once.** `child_preamble` composes `ACT`; the child line, or for a root "the
  program that owns this session constructed you; no user reaches you"; the docstring as written;
  each section as `## {title}`, a blank line and its text, verbatim; the inputs; the `self`
  sentence; the facility entries; the channels and how to answer; the skill catalog for an
  owner-constructed root; and `rest()`. It happens once, before the orienting round, because the
  preamble is given to the model once (`build_agent`) and recorded once as `model.instructions`; a
  task that changes is carried by the task object, which the model reads in code. Nothing is read
  from the instance but what the hook returns, so no `repr`, attribute or property runs: the test
  with a facility whose `__repr__` writes a file shows it, `0003-42`'s manifest reading only
  class slots.
- **The facility manifest is split.** `facilities()` became `facility_entries`, the header and
  each entry, composed before the channels, and `facility_rules`, how each kind is used, the
  hosted-call rule and the global-name sentence, which stays in `rest()` with the rule for a
  binding, as `task-agents.md` has it. A child with no mapping keeps the session's bindings and
  services text in `rest()`, as the primary does: there is no mapping to list, and the primary's
  text is byte-identical. `skilled()` took a `directive` flag so the primary's `/name` paragraph,
  which needs a user channel, is left out of a root's catalog.
- **Roots.** A root is a child of the owner (`parent == OWNER` in `make`): both kinds -- the owner's
  in its kernel and a host program's class -- are told that the program that owns the session
  constructed them and that no user reaches them. Only the owner-constructed root has `self` in
  its kernel, so only its `self` sentence takes the root form: the instance that program
  constructed, no "your parent's code holds `self`" (the owner holds a handle, not the object),
  and it runs until that program releases it. Only that root, the application's primary, is told
  the session's skills, under `0003-32`'s bound, through `ChildSetup.skills` and
  `children::Settings.skills`; a child is told none. A root that declares no request channel is
  told so, with no channel list and no how-to-answer paragraph -- it keeps the `runtime.wait`
  and round-end sentences -- since the text for a class with channels would list none.
- **The root's lead (fork 3, as recommended).** `ORIENTING`, then, for a root whose class
  contributed sections, "Your task is in your instructions, and this round is where you begin it".
  The runner gets a `task` flag for it from `make`.
- **The opening's order.** The orienting round's opening line is `orienting(task, waiting)`: the
  lead, then what already waits, by channel and count, from the registry's `unread_text`, the
  words every later round's opening uses. It applies to every instance's orienting round, the
  runner's being shared, and `agent-classes.md` says so. A request body is never in the
  instructions; the orientation says in one sentence that a round's opening line says what waits,
  by channel and count, and never what it holds, and that each request is read in code with
  `receive()`. Empty initial input announces nothing and invents nothing: the test compares the
  opening with the function's own output. A request already waiting when the runner first looks
  cannot be set up from outside: the registry starts the runner before it answers the owner or the
  constructing kernel, so a request made through the handle arrives after the lead was composed.
  The test therefore uses `runner::HOLD`, a test-only thread-local that the next runner to start
  waits on before its first round, and makes the request, once the registry has recorded it,
  before it lets the runner go. `0003-51`'s window will hold a delivery before the root launches,
  and needs no hold.
- **`help(outrig.Agent)` had no room left.** It was 8,141 of `HELP_MAX`'s 8,192 characters. The
  hook's paragraph is paid for by tighter wording in the class's docstring, the `facilities`
  property and `ready()`, no fact removed that a test or a page relies on, and by the default
  hook having no docstring; it ends 33 characters under the bound, measured with pydoc's plain
  renderer as 0003-45's note did. The bound itself is unchanged.
- **No close row, no new event.** A hook runs inside construction, which `0003-41`'s and
  `0003-46`'s rows cover: the Executions rows for a kernel's child, and `agent.construct.failed`
  for a root. `agent.instance.started` gains `contributions`, each `{title, length}`;
  `model.instructions` already carries the composed text. `diagnostics.md` asks for a record of
  which part came from OutRig, the class, the contributions and the manifest: the event's
  `contributions`, `channels` and `facilities` with the instructions say it, so no per-part
  record is added.
- **The application supplies content and OutRig the assembly.** No Python API takes prompt text
  whole, an order or a separator: a section is a title and a text, composed where OutRig puts it,
  and what an application omits from a child's mapping is not advertised to that child.
- **`/simplify` adopted part of an independent implementation.** Built from the same requirement, it
  was a little smaller and differed in shape. Adopted: `ChildBrief::Instance` as a struct variant
  with `root: bool` and `skills`, instead of a wrapper type and an `Option` that said both; the
  sections as borrowed `(&str, &str)` pairs read inline from the line, instead of an owned type and
  a reader function; one prefix that names the section in its three `ValueError`s; and no docstring
  on the default hook. Kept: the dedicated match arm for a root with no request channel, since the
  alternative's prompt still told that root a request could wake `runtime.wait` and that a sender
  would get an exception; the facility entries and rules as two functions instead of a `listed` flag
  threaded through `rest()`; `orienting()`; and the root's own `self` sentence. The alternative left
  out what `help(outrig.Agent)` says of the hook, and the sentence about what already waits was
  added only for a root that had sections. Its iterable test, `hasattr(returned, "__iter__")`, was
  adopted and then dropped on review: it refused a class that defines only `__getitem__`, which
  `for` and `list()` accept. The test is `iter(returned)`, with a `str` or `bytes` refused first and
  a `TypeError` from `iter` raised as the hook's own, chained.
