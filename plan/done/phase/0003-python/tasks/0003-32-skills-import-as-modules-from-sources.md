# 0003-32 -- Skills import as modules from pluggable sources

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
  as how to use one. `0003-33` adds what a `/name` message asks.
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
  answers `get_source`, so tracebacks show a skill's lines and `0003-31`'s body check can read a
  declaration made in a skill. What `__file__` is, per fork 6.
- **A digest per skill**, recorded with the skill's name and source as `__outrig_skill__` on its
  package when the package is imported. Fetching any further file of a skill whose digest has
  since changed raises `skill-changed`, so two versions of one skill are never mixed; `0003-33`'s
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
   `0003-33`, numbered by `/groom-plan`, and its acceptance items go with it.

## Dependencies

- **Hard: `0003-19`.** The trait and the builder method belong to the embedding API.
- **Hard: `0003-26`.** `outrig.skills` is part of the `outrig` package.
- **Soft: `0003-16`.** A fetch is a request a kernel thread blocks on while the reader thread
  brings the answer, and `0003-16`'s `rpc` message kind is the first such request; reuse its
  mechanism where it fits.

## See also

- `plan/phase/0003-python/skills.md` -- layout, sources, discovery, the preamble, the loader,
  reload and dependencies, and the open questions this task's forks answer.
- `plan/phase/0003-python/discovery.md` -- what the preamble carries.
- `crates/outrig/src/agent/orientation.rs` -- the preamble this extends.
- `plan/next/skill-dependency-installation.md` -- installation, deferred.

## Decisions

Forks 2, 4, 5, 6 and 7 were taken as recommended, fork 1 as the maintainer chose, and fork 3 as
recorded below. What no doc settled is recorded here.

- **Fork 1: `saphyr-parser` 0.2.2 (the maintainer's call).** Its own runner passes the official
  yaml-test-suite with no exclusions of its own, as yaml-rust2's does; saphyr is the line under
  development. The official matrix (data of 2022) lists only the original yaml-rust, at 317 of
  402, before both forks' fixes; serde_yaml was archived in 2024 and its libyaml translation
  scores 330 of 402. Frontmatter is read one event at a time, never through the tree loader, which
  recurses once per nesting level with no limit. An alias, a nonzero anchor id, a duplicate
  top-level key, a key that is not a scalar, a second document, nesting past 32, or YAML that does
  not parse refuses the skill as `metadata-invalid`; tags are ignored, and every key but `name`
  and `description` is read and ignored whatever its shape, so another tool's fields cost nothing.
  A plain `~`, `null` or empty value is no value. The scanner caps flow nesting at 255 itself, and
  the file is bounded at 8 MiB before it is parsed.
- **Fork 2.** A directory source's version is `<size>:<mtime>.<nsec>:<inode>`: the inode catches a
  same-size replace within one timestamp tick, which ext4's coarse clock allows. The digest is
  blake3 over every listed path and version, length-prefixed, in path order. A load re-reads
  `SKILL.md` and `skill.py` only when the digest has moved since they were last read. Every
  answer that reads a file lists the skill again after the read and is refused as
  `skill-changed` when the digest moved, whatever the read returned: a file saved while it was
  read would otherwise be served under the old digest, part one version and part the next, or
  its failed read reported as the skill's fault. A load keeps the metadata it read only once
  that check passes.
- **Fork 3: `pip._vendor.packaging` from the payload's pip** (26.2.1, `packaging` 26.2), the only
  option with no build step, crate, mount or protocol. Checked under the payload's 3.13.15: it
  imports, evaluates markers, and loads no `pip._internal`. The payload's site-packages precedes
  the environment the agent's pip installs into, so `pip install pip` cannot replace it. Markers
  are evaluated, a requirement whose marker is false is skipped, and `prereleases=True` is passed,
  since a check is not a resolver. Extras are not followed into the distribution's own
  requirements. A specifier that does not parse is `metadata-invalid` at import.
- **Fork 4.** The first source that lists a name decides it even when that copy proves invalid:
  the project's broken skill is reported, never silently replaced by the user's. Every later copy
  is `shadowed` and never read.
- **Fork 5.** At most 50 skills or 12 KiB of them, whichever comes first, then a count and
  `outrig.skills.catalog()`. The bytes bound is there because the standard allows 1,024-character
  descriptions, which 50 of would cost some 13,000 tokens a call. Descriptions are put on one line
  in the preamble. A child is told none.
- **Fork 6.** No `__file__`; the code's filename is `<skill NAME>/PATH`, relative and not
  angle-bracket-wrapped, and the source is registered in `linecache` with `mtime` `None`, which
  `checkcache` keeps, so a traceback or `inspect.getsource` never looks for the path in the
  workspace. The loader keeps the source before the module runs, so `@outrig.agent`'s digest of a
  declaration made in a skill (`0003-31`, which reads `get_source`; its body check is gone) fetches
  nothing more. Checked under the payload's Python before any code was written.
- **One module, `outrig/skills.py`**, rather than a public module beside a private `_skills.py`:
  its machinery is underscore names, `__all__` keeps them out of `help()`, and no circular import
  arises. `Origin` and `Skill` are slotted classes, as `outrig.schema`'s `Problem` is: as frozen
  dataclasses they put `help(outrig.skills)` at 9,391 characters, past the 8 KiB bound.
- **The trait** is `skills()`, `files(skill)` and `read(skill, path)`, synchronous, with `io::Error`
  for failures and `SkillFile { path, version }` its one listing type. A separate `files` lets the
  host re-list one skill on every read, which a whole-source listing would make cost every skill.
  Each operation runs on a thread of its own within 30 seconds, the wait for its place included,
  at most 8 at once per session -- discovery's and serving's alike -- the permit going with the
  thread so a source that never returns keeps its place rather than taking more threads. Not on
  tokio's blocking pool: dropping a runtime waits for every task on it, so a source stuck on a
  mount that stopped answering would hold `run-new`'s exit.
  Discovery lists every source at once and then reads every skill at once, so a source that hangs
  costs the start one bound, not one per skill. Past the bound the answer is `source-error`, so an
  embedder's hung service never strands an import. The host applies the source's own bounds to
  any source: 1,024 files, valid paths with no duplicate, 8 MiB per read.
- **`DirectorySource` follows no link below its root.** The root is the operator's path, opened at
  first use and held, so swapping `.agents/skills` mid-session changes nothing. Below it every
  component is opened `O_NOFOLLOW | O_NONBLOCK | O_NOCTTY` and never `O_DIRECTORY`, so a link of
  either kind fails as `ELOOP` and is said to be one (`O_DIRECTORY` makes a linked directory
  `ENOTDIR`); `fstat` must then say regular file. A listing reads each entry with `fstatat`,
  following nothing. A link inside a skill is listed, so reading it fails saying it is a link
  rather than the file seeming absent; a skill directory that is a link is `source-error`.
  Installing a user skill by symlinking its directory into `~/.agents/skills` therefore does not
  work; symlinking the whole directory does. `openat2(RESOLVE_BENEATH)` would have allowed links
  that stay inside, at the cost of Linux 5.6 and a fallback. A skill's files may be 16 directories
  deep; a 17th is refused. Names beginning `.` and `__pycache__` are not served: a checkout's
  `.git` would pass the entry bound, and a local run's bytecode would move the digest.
- **`DirectorySource::within(base, path)`**: a repository can commit `.agents/skills` as a link to
  another checkout's skills (the design review found it). The root's real path is read back from
  `/proc/self/fd` after it is opened and must lie inside `base`, so the check is of what was
  opened; a link inside the repository works. A missing root holds no skills.
- **Discovery** checks the directory's name against the standard's rule first and reads nothing
  of a name that fails it. Three codes join the task's two: `shadowed`, `unknown-key` (the skill
  is still served) and `source-error`. Every untrusted name in a message is quoted with `{:?}`, and
  a diagnostic's message, like an import's reason, escapes each control character but a line
  break or a tab: a TOML or YAML parser's error quotes the line it stopped at, which can hold a
  terminal's escape sequences, and the line mode writes notes to stderr as they are.
  `[tool.outrig.skill]` knows `entry` and `bind` only, as
  the task says; `parameters` warns until `0003-33`, whose task says it will teach the parser that
  key. An unknown key directly in `[tool.outrig]` warns too. Neither `entry` nor `bind` is kept
  yet: `0003-33` reads them.
- **The PEP 723 regex** agrees between Rust's `regex` and Python's `re` on blank-separated blocks
  (two), adjacent blocks (one block, whose `///` lines are not TOML), unclosed blocks and CRLF
  (none) -- checked in both engines. CRLF and a BOM are normalized first, as Python's text mode
  would.
- **The wire.** The host routes any `skill.`-prefixed line ahead of the `Envelope` parse, whose
  `agent` is required, since code outside every execution names none; the hook is cloned out of
  the table in its own statement (the `children` route's `match lock(..)` holds the guard through
  its arms), and answers go through the weak writer `relay_line()` gives. A session given no
  source still answers each request, `unknown-skill`, so no thread waits on one for good.
- **The interpreter's half is small** (its `-c` bootstrap was 108,790 of 131,072 bytes):
  `_ask_host` and `_compiled_cause`, with everything else in the package. Each request has a
  queue of its own, which `_route` hands the answer to; one that comes after its asker gave up
  finds none and is dropped. A request is refused on the reader thread and in a forked child; the
  wait looks every `SIGNAL_CHECK` seconds; a 60-second backstop ends a wait whose answer was lost.
  Running the full suite found an interrupt lost under load: the SIGINT landed while the main
  thread was still in `_write_line`, which `_on_sigint` treats as machinery and dropped it. A
  request agent code makes now leaves the handler a thread-local list before it writes, as a
  hosted call leaves its wait; the handler puts its `KeyboardInterrupt` there and records it as
  landed, so one that ends a background task is reported to its owner, and the wait raises it in
  preference to any answer. A request this program makes for itself is not an interrupt's to end.
  A child's kernel takes no SIGINT; its wait ends by the answer or the backstop (#699).
- **The finder never asks the host**: the import system holds its global lock across every
  `find_spec`, so modules are found from the catalog and the listing the package was imported
  with, and fetched in `exec_module`, under that module's lock alone. A test shows another kernel
  importing while one waits. One loader per import of a skill's package serves the package and
  each of its modules, holding the listing and digest it was imported at and each module's
  source; a directory's `__init__.py` wins over a module of the same name, as with Python's own
  finder. `outrig_skills` is a spec with no loader, as a namespace package is, and a node of the
  listing is its own resource reader, its `read_text` taking the `errors` that
  `importlib.resources.read_text` passes on 3.13 and `Traversable`'s own lacks.
- **What a skill's code raises while it is imported is raised as it was, whoever imports it.**
  Raising a `SkillError` in its place broke a skill's own `except` -- the fallback for an optional
  sibling, or an `except ImportError` that then swallowed a sibling's `ValueError` -- and sparing
  only the skill's imports meant telling whose each import was: a walk of the frames, then hooks
  on threads, pools and tasks, and still wrong for work handed between them. What it bought was a
  type agent code could catch, which little code does. So a failure is labeled where it is
  reported, as `0003-10` decided for every import: `_explain_imports` says when compiled code is
  why, reading the skill's listing as well as the disk, and `skill.imported` carries
  `import-failed` or `native-extension-unavailable`. `SkillError` is what OutRig's own checks and
  fetches raise. A package that fails is labeled before its listing goes, the listing being
  evidence, and takes every module below it out of `sys.modules`, so its next import fetches each
  again at the files' new digest; Python's own import leaves such a module in place. The native
  evidence is `0003-10`'s, now also read through `name_from`:
  `from . import fast` swallows the `ModuleNotFoundError` and raises an `ImportError` naming the
  parent and `name_from`, with no cause, so the chain held no evidence. `_compiled_cause` is
  factored out of `_explain_import`, which as a result explains the `from p import m` form too. A
  compiled file in the skill's own listing where the import looked is evidence as well. A failed
  package's listing is gone by the time its failure is reported, so the report finds it through
  the frames the exception passed, each skill module's `__spec__` holding its loader. The chain
  is followed through every link, so an `ImportError` a skill wraps in another exception still
  counts; only `ImportError` and `ModuleNotFoundError` themselves are read, and a notice names the
  raised type and never its text, so labeling runs no agent code.
- **Events.** `skill.discovered` is published only when the builder was given a source.
  `skill.imported` is one per package import, success or not -- a module that fails later is
  raised to the code that imported it, not evented -- with the importing agent as its subject, or
  none outside every execution.
- **The preamble.** `Agent::build` was at clippy's seven arguments, so the bindings and skills
  travel as `orientation::Holdings`; the skill briefs go to `build_primary` through `assemble`
  rather than being kept on `Session`, which would have taken `Session::new` to eight.
- **The CLI** gives `DirectorySource::within(repo_root, ".agents/skills")`, then the user's
  `~/.agents/skills` unless the home is the repository root -- a run from `~` with
  `~/.agents/outrig/config.toml` makes it so, and every skill would shadow itself. Diagnostics
  print as `[outrig]` notes in line mode and as inline notes on the screen, right after the
  banner.
- **Host programs get no skill sources**: giving them the CLI's would add a public method to
  `HostProgram`, outside the additions the acceptance allows, and `embedding.md` lists skill
  sources among what a program lacks in this phase (#698).
- **Tests.** Discovery, the parsers and `DirectorySource` in `harness/skills_tests.rs`, with the
  host's answers driven through `testing::Fake` and the loader through an interpreter on the host
  and a `Counting` source; withheld and late answers, an interrupt, another kernel's import and a
  fork in `interpreter_tests.rs`; the preamble with a mock model in `agent_tests.rs`, this
  repository's own skills included; and two e2e tests, one in `agent_tests.rs` comparing a session's
  mounts with and without a source while its agent imports a skill in Alpine, one through
  `run-new` in `run_new_e2e.rs`.
