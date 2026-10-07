# 0003-24 -- `outrig` imports in every kernel, and decodes results strictly

## Context

Agent code reaches OutRig through `runtime`, a global each kernel's namespace is given at boot.
That serves code typed into an execution and not code in a module: a skill, or a module in the
workspace, has globals of its own with no `runtime` in them. `import outrig` is how module code
gets there, and the runtime it reaches has to be the calling execution's. `sys.modules` is
interpreter-wide, so one module object serves every co-hosted kernel.

`interpreter.py` already tracks which execution is running: `_CURRENT` is a context variable set
at the top of each execution's task. A task, a `threading.Thread`, and an item given to a thread
pool -- `asyncio.to_thread`'s included -- carry the context they were started or submitted from
(#474), so each can resolve its execution's runtime. A thread started with
`_thread.start_new_thread`, or a pool's `initializer`, runs outside every execution's context and
has nothing to resolve -- which its error has to say.

The second half is the decoder typed results need (`typed-agents.md`). A child's completion is JSON
that must become the declared dataclass strictly: an unknown key is an error rather than dropped, a
field the schema requires must be present even when its value may be `null`, and defaults come
from the dataclass and nowhere else. The type's schema, with its field documentation intact, is
what a child is told to produce. The maintainer's `ReviewResult` in `typed-agents.md` is the case
that has to pass as written, and it uses two things the serializable subset (`messages.md`, checked
by `_check_subset` since `0003-08`) does not cover: `Literal` and `Annotated`.

A provider's strict mode cannot stand in for the decoder. Anthropic's and OpenAI's reject
map-valued schemas, and `ReviewResult` has three maps. The schema reaches a child as text in its
instructions, and the decoder is what enforces it.

## Goal

Module code reaches the calling execution's runtime through `import outrig`, and a JSON value
becomes a declared dataclass only when it has exactly the declared shape.

## Deliverables

- **An `outrig` package in every kernel**, including one opened after the first. Where its source
  lives per fork 1. It holds OutRig's own code and nothing vendored: RPyC is vendored by
  `build.rs` into a directory of its own, mounted read-only beside the payload (`0003-16`).
- **`outrig.runtime`**, the current execution's runtime, resolved from `_CURRENT` per fork 2. The
  kernel's `runtime` global stays. Outside every execution's context -- a thread started with
  `_thread.start_new_thread`, a pool's `initializer` -- it raises an error saying why and naming
  `threading.Thread`, `asyncio.to_thread` and `contextvars.copy_context().run` as the ways to
  carry the context.
- **`outrig.schema`**, with `doc(text)` and `alias(name)` markers for `typing.Annotated`. A `doc`
  text is the field's description, and an `alias` is the field's JSON key. Nothing else renames a
  field: there is no hyphen-to-underscore conversion in either direction.
- **The types it takes**: the serializable subset -- `str`, `bool`, `int`, finite `float`, `None`,
  `list[T]`, `dict[str, T]`, optionals, declared unions, and dataclasses made of these -- plus
  `Literal` of string, integer and boolean values, and `Annotated` around any of them. Hints are
  resolved in the declaring module's namespace, with
  `typing.get_type_hints(..., include_extras=True)`, so `from __future__ import annotations`
  works. A name that does not resolve, or a type outside the set, fails when the type is declared,
  naming the field, and not at the first decode.
- **Strict decoding into the dataclass**: an unknown key is rejected, including a field's own name
  when the field has an alias; a required field must be present, a required-but-nullable one
  included; `null` is accepted only where the type includes `None`; an omitted field takes its
  dataclass default or `default_factory` and nothing else; a `Literal` matches by value and type,
  so `true` is not `1`; an integer passes as a float, as `_conforms` already allows. Every problem
  in a value is reported, each with a JSON path to where it is, such as
  `$.notes['unbounded-retry-loop'].severity`, up to a bound on how many.
- **Schema text**: a JSON Schema rendering with every `doc()` text intact as `description`, each
  `Literal` as an `enum`, each dataclass an object with `additionalProperties: false` and every
  field without a default in `required`, and `dict[str, T]` an object whose `additionalProperties`
  is `T`'s schema. `0003-25` and `0003-26` put this text in a child's instructions.
- A way to declare a type, checking it at once; to decode a JSON value against it; and to render
  its schema text. Their spellings are this task's, and `help(outrig.schema)` documents them.

## Acceptance

- `import outrig` works in the first kernel and in one opened later, and `outrig.runtime` is that
  kernel's runtime in each.
- A function in a module imported once, called from two kernels, reaches each caller's runtime
  through `outrig.runtime`.
- **In a `to_thread` worker and a `threading.Thread`, `outrig.runtime` is the calling execution's
  runtime**; in `contextvars.Context().run(...)`, outside every execution's context, it raises the
  documented error, not an `AttributeError` and not `None`.
- **The maintainer's `ReviewResult`, copied from `typed-agents.md` unchanged, declares without
  error**, and its example JSON decodes to the instance the example describes. Its schema text
  carries every `doc()` text verbatim.
- **The shape cases**, each a separate assertion:
  - every `Literal` value round-trips, `reject-changes`, `not-found` and `unknown` included;
  - `pre_existing_issues` as a JSON key is rejected as unknown, since its alias is
    `pre-existing-issues`;
  - omitting `Note`'s `duplicate-status` or `duplicate-reference` is rejected as a missing
    required field;
  - `null` for `duplicate-reference` is accepted, and `null` for `Note`'s `location` or
    `severity` is rejected;
  - `null` for `Question`'s `location` is accepted, and omitting the key is rejected -- the field
    is required-but-nullable, not optional;
  - omitting `questions` is the only omission accepted anywhere, and it gives an empty map;
  - a missing `verdict` is rejected;
  - two decoded instances never share a default container;
  - no invented policy: `reject-changes` with only `low` notes decodes, and a question whose id
    matches no note is accepted.
- A value with several problems reports all of them, each with its JSON path.
- **A non-serializable annotation fails at declaration** -- `bytes`, `set[str]`, a class that is
  not a dataclass -- naming the field, and so does a hint that does not resolve.
- **A dataclass built at runtime declares, decodes and renders.** One made with
  `dataclasses.make_dataclass`, so it has no source, declares without error, decodes its JSON form
  to an instance, and renders schema text listing its fields; `0003-29`'s generated message type
  is one.
- **Channel contracts take the same types** (with fork 4's recommendation): a channel whose
  contract is a dataclass with a `Literal` field and an `Annotated` one is accepted, a message that
  conforms to it is delivered, and a message whose `Literal` field holds a value outside the
  `Literal` is refused when it is sent.
- `crates/outrig/public-api.txt` is unchanged: this task adds no Rust surface.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **Where the package source lives -- Recommended: synthesized by `interpreter.py`**, so the
   payload stays one versioned program and the `outrig` module cannot be a different version from
   the runtime it reads. Files on the payload mount, imported normally, would give tracebacks with
   source lines and source for `inspect`, at the cost of a second artifact whose version must
   match. Either way the name `outrig` shadows any package of that name an agent installs; whether
   to register it on PyPI, so no other package takes it, is a separate call for the maintainer.
2. **What `outrig.runtime` is -- Recommended: an object that resolves the current execution's
   runtime on each attribute access.** `from outrig import runtime` at a module's top level runs
   once, in whichever kernel imported the module first, so with the object it still reaches each
   caller's runtime. A module-level `__getattr__` returning the runtime itself is simpler, and binds
   the first importer's runtime into every module that uses the `from` form. The object's cost:
   `type(outrig.runtime)` is not the runtime's class, which its docstring states.
3. **How a union of dataclasses decodes -- Recommended: a value decodes against a union only when
   exactly one member accepts it**, and otherwise the problem names the members that accepted it,
   or says none did. A message on a channel carries an identifier for its type (`messages.md`) and
   a model's JSON does not, so decoding by the first member that fits would choose by declaration
   order. `ReviewResult` has no such union.
4. **Whether channel contracts take `Literal` and `Annotated` too -- Recommended: yes, one
   subset.** `_check_subset` and `_conforms` already define the subset for channels, and widening
   them once keeps a channel contract and a completion type from accepting different types.

## Dependencies

- **Hard: none outstanding.** It extends `0003-08`'s subset check and reads the per-execution
  context the interpreter has had since `0003-02`, both done.

## See also

- `plan/phase/0003-python/typed-agents.md` -- `outrig.schema`, the decoding rules, and the
  maintainer's `ReviewResult`.
- `plan/phase/0003-python/messages.md` -- the serializable subset.
- `crates/outrig/src/python/interpreter.py` -- `_CURRENT`, `_check_subset` and `_conforms`.
