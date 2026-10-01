# 0003-27 -- Skills import as modules from pluggable sources

## Context

A skill today is a directory holding a `SKILL.md`: YAML frontmatter with a name and a description,
then instructions. This repository has two, `.agents/skills/groom-plan` and
`.agents/skills/next-task`, in the Agent Skills format, which several agent tools read from
`.agents/skills`. `skills.md` adds an optional `skill.py`, a module the agent imports and calls
beside the instructions.

Planning settled the shape:

- **Two roots.** `<repo-root>/.agents/skills/<name>/` for the project and `~/.agents/skills` for
  the user. A name in both is served from the project, and the shadowing is reported.
- **Every skill comes from a source** -- the CLI's two, or one an embedder supplies -- and its
  files reach the container over the interpreter's pipe when they are used. No skill directory is
  mounted.
- **Discovery runs on the host and runs no Python.** It reads the frontmatter and `skill.py`'s one
  `# /// script` block (PEP 723), whose `[tool.outrig.skill]` table carries `entry` and `bind`.
- **The preamble lists each skill's name and description**, which is what the Agent Skills
  standard loads at startup; a skill's body is read when the skill is used.
- **Dependencies are checked, never installed.** `plan/next/skill-dependency-installation.md`
  records what installing would take.
- **A skill is a module**, not code run into the agent's globals: `outrig_skills.<name>`, its
  siblings importable relatively, and nothing added to `sys.path`.

One consequence is ordinary Python and is documented rather than prevented: `sys.modules` is
interpreter-wide, so co-hosted agents share one module object per skill, and its module-level
state with it.

The sources read files on the host with the user's authority, which is what makes one rule
necessary: a source serves only files inside the skill's own directory. Without it, a symlink in a
repository's skill that resolves to `~/.ssh` would hand a host file to the container.

## Goal

The agent sees which skills exist without any of their code running, and imports one as a module
whose files arrive when they are used.

## Deliverables

- **A `SkillSource` trait on the embedding API**: list the skills a source has, and read one file
  of one skill. A builder method takes sources in precedence order, per fork 4. OutRig provides
  the directory source the project and user roots both use, and an embedder may add its own. The
  CLI supplies the project source, then the user source; a name in both is served from the
  project, and startup reports the user's copy as shadowed.
- **The directory source serves only what is inside the skill's directory.** A path that resolves
  outside it, through a symlink or `..`, is refused, and the number and size of the files it
  serves are bounded.
- **Host-side discovery, with no Python.** `SKILL.md`'s frontmatter is parsed per fork 1 and checked
  against the standard: a `name` of 1 to 64 lowercase letters, digits and hyphens, with no hyphen
  first, last or next to another, matching its directory, and a `description` that is present.
  `skill.py`'s `# /// script` block is found with PEP 723's regular expression and parsed as TOML
  with the `toml` crate already in the tree. Only the file's leading lines are searched -- those
  before its first line that is neither blank nor a comment -- so text that looks like a block
  inside a docstring, or anywhere after the first statement, is never read, and discovery stays
  lexical. A block with no closing `# ///` is not a block, and is ignored. `[tool.outrig.skill]`
  takes `entry`, the entry function's name, `main` when absent, and `bind`, a table from entry
  parameter to binding name; an unknown key is a warning. A skill that fails is left out of the
  catalog and reported with its code: `metadata-invalid`, or `duplicate-metadata-block` for two
  `script` blocks, as PEP 723 requires of tools.
- **The preamble** lists each skill's name and description, per fork 5, and names `outrig.skills`
  as how to use one. `0003-28` adds what a `/name` message asks.
- **`outrig.skills.read(name)`** returns a skill's `SKILL.md` body, the accessor `skills.md` leaves
  to this task -- for an instruction-only skill the agent judges relevant, or for an executable
  skill's own instructions.
- **An import finder in the container** that serves `outrig_skills.<name>`, hyphens becoming
  underscores, as a package whose files are fetched from the source over the pipe as they are
  imported: `skill.py` as `outrig_skills.<name>.skill`, and its siblings by relative import. Every
  source's files arrive this way, the project's included; none is read through a mount, the
  workspace mount included, so a session's mounts are the same with skills as without, as planning
  settled (`skills.md`). The skill's directory is never on `sys.path`, so a skill's `json.py` is
  `outrig_skills.<name>.json` and nothing else. The loader supplies a resource reader, so
  `importlib.resources.files()` on the package reads data files through the same fetch, and it
  answers `get_source`, so tracebacks show a skill's lines and `0003-26`'s body check can read a
  declaration made in a skill. What `__file__` is, per fork 6.
- **A digest per skill**, recorded with the skill's name and source as `__outrig_skill__` on its
  package when the package is imported. Fetching any further file of a skill whose digest has
  since changed raises `skill-changed`, so two versions of one skill are never mixed; `0003-28`'s
  `invoke` checks the same digest. `outrig.skills.reload(name)` removes exactly
  `outrig_skills.<name>` and its dotted descendants from `sys.modules` -- never by string prefix,
  which would take `review_diff` along with `review` -- and imports afresh. References to the old
  code keep the old code, which is documented. How a change is detected, per fork 2.
- **Dependencies checked, never installed**, when the package is imported: `requires-python`
  against the interpreter, and each entry of `dependencies` against the distributions
  `importlib.metadata` finds. Each failure has its own code: `requires-python`;
  `unsatisfied-dependency`, naming each unmet requirement; `import-failed`, with the original
  traceback, for anything raised while importing; and `native-extension-unavailable` only where the
  failure is verified to be a compiled module, on the evidence `0003-10`'s import diagnostic uses.
  Evaluating requirement specifiers per fork 3.
- **Events**: discovery's result and diagnostics, and each import and reload of a skill with its
  digest.

## Acceptance

- **Listing runs no skill code**: a skill whose `skill.py` writes a sentinel file when imported
  leaves no file after the session starts and lists it.
- **A counting source sees no read of a module or data file until import.** Listing reads
  `SKILL.md` and `skill.py` and nothing else; a sibling module is read when it is imported, and a
  data file when `importlib.resources` opens it.
- **Sibling modules import** by relative import, and a skill's `json.py` does not shadow the
  standard library's `json`, in the skill or in the agent's own code.
- **A changed file raises `skill-changed`**, and `outrig.skills.reload("review")` leaves
  `outrig_skills.review_diff` loaded, the same module object as before.
- **An unmet dependency is a structured error**: `unsatisfied-dependency` naming the requirement,
  with nothing installed and pip never run; `requires-python` for a block that asks for a newer
  interpreter; `import-failed` for a skill that raises while importing.
- **Import failures are labeled by cause.** A skill whose import reaches a compiled module the
  interpreter cannot load -- a file with an extension suffix where the import looked -- fails with
  `native-extension-unavailable`. A skill importing a sibling module that does not exist, and one
  importing a package it does not declare and that is not installed, each fail with
  `import-failed` and the original traceback, and neither is labeled native.
- **This repository's own `.agents/skills/groom-plan` and `.agents/skills/next-task` appear in the
  preamble** with their descriptions, read through the project source.
- A skill in both roots is served from the project, and the user's copy is reported as shadowed.
- **Metadata is read only before the code.** A `# /// script` block after the module's first
  statement, including one inside a docstring, is not read; a block with no closing `# ///` is
  ignored, and the skill takes the defaults; and a file with two `script` blocks is left out with
  `duplicate-metadata-block`.
- A frontmatter `name` that does not match its directory is left out with `metadata-invalid`, and
  an unknown `[tool.outrig.skill]` key produces its warning.
- **A symlink out of a skill's directory is refused**: a skill holding a link to a file elsewhere
  on the host cannot have that file read, by import or by `importlib.resources`.
- A session with skills mounts nothing it would not mount without them.
- `crates/outrig/public-api.txt` regenerated: the trait, its listing types, the directory source,
  the builder method, and the skill events are its only additions.
- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --check` pass.

## Design forks

1. **How frontmatter is parsed -- Open: a YAML crate, or a subset parsed by hand.** No YAML crate is
   in `Cargo.lock` today. The standard's fields are strings, except `metadata`, a map from strings
   to strings (agentskills.io/specification), so plain and quoted scalars plus one level of string
   map cover it, and this repository's two skills use plain scalars only. Frontmatter can be
   repository content, and it is parsed on the host, in the owner's process, so what a parser
   refuses weighs as much as what it reads. Anchors and aliases are the main risk: a few lines of
   nested aliases expand exponentially when a parser builds out the values, so the parser refuses
   them, or bounds how far they expand. A subset does so by having no anchors or aliases at all; a
   crate has to be shown to refuse or bound them, or be wrapped in a check that does. Either way the
   frontmatter is bounded in size before it is parsed -- the source already bounds a file -- and the
   parse in time and in nesting depth, which a single pass written by hand over bounded input meets
   by construction. A subset fails on YAML a skill author can reasonably write -- a block scalar for
   a long `description` is the likely case -- which a crate would read. The question is whether that
   case is worth a dependency and the checks it then needs.
2. **How a change is detected without reading every file -- Recommended: a version per file in
   the source's listing, with the digest over every file the source lists for the skill.** Data
   files count, since a skill reads its data through `importlib.resources`. For the directory
   source a file's version is its size and modification time, read without opening it; an
   embedder's source supplies its own. Hashing contents is exact, but it reads every file at
   import, which fetching files as they are used is meant to avoid, and again at each check.
3. **Where requirement specifiers are evaluated -- Open.** Comparing a PEP 440 specifier with an
   installed version needs code the standard library lacks. The payload's pip carries `packaging`
   as `pip._vendor.packaging`, an internal module of pip but pinned with the payload; `0003-16`'s
   vendoring mechanism could add `packaging` itself; or the host could compare, given the versions
   `importlib.metadata` reports in the container.
4. **How an embedder's sources rank against the project's and the user's -- Recommended: one
   ordered list, the first source holding a name winning**, which extends "the project wins". The
   CLI's list is project then user; an embedder places its own sources where it chooses, and every
   name a later source loses is reported as shadowed.
5. **What the preamble lists -- Recommended: every skill for the main agent, none for a child,
   and a cap with a count for a large catalog.** A child can still use every skill, since the
   finder serves the whole interpreter, but a list costs tokens on every model call of every child,
   and a child made for one typed call rarely needs it; its parent's prompt can name a skill. At
   about 100 tokens per skill, a hundred skills cost about 10,000 tokens a round, so past a fixed
   number the preamble lists that many, says how many more there are, and leaves the rest to
   `outrig.skills`, as `discovery.md`'s inventory does for names.
6. **What `__file__` is on a skill module -- Recommended: unset.** A skill's files are not in the
   container, so a path built from `__file__` would name nothing, and code that tries fails at
   once rather than reading the wrong file. Tracebacks still name the skill through the compiled
   code's filename.
7. **Whether the loader moves to a task of its own -- Recommended: keep this task whole unless
   execution shows it is too large.** The line runs between the host side -- the sources,
   discovery, the preamble and `outrig.skills.read` -- and the container's loader -- the finder,
   resources, the digest and reload, and the dependency check. The host side can be tested with
   no import at all: the preamble lists the skills, and `read` returns a body. The loader needs the
   host side's sources to fetch from. Split, the loader becomes a task between this one and
   `0003-28`, numbered by `/groom-plan`, and its acceptance items go with it.

## Dependencies

- **Hard: `0003-19`.** The trait and the builder method belong to the embedding API.
- **Hard: `0003-24`.** `outrig.skills` is part of the `outrig` package.
- **Soft: `0003-16`.** A fetch is a request a kernel thread blocks on while the reader thread
  brings the answer, and `0003-16`'s `rpc` message kind is the first such request; reuse its
  mechanism where it fits.

## See also

- `plan/phase/0003-python/skills.md` -- layout, sources, discovery, the preamble, the loader,
  reload and dependencies, and the open questions this task's forks answer.
- `plan/phase/0003-python/discovery.md` -- what the preamble carries.
- `crates/outrig/src/agent/orientation.rs` -- the preamble this extends.
- `plan/next/skill-dependency-installation.md` -- installation, deferred.
