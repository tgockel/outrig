# AI-assisted container design -- `outrig mcp design`

> **Status:** preliminary spec. Carved into a numbered task in `plan/todo/`
> when ready.

## Context

`outrig container add` is template-driven. It walks the user through a
fixed menu of base images (Debian Bookworm, Ubuntu 24.04, Alpine, Node 20,
Python 3.12), language toolchains (Rust / Node / Python / Go / none), and
built-in MCP server presets (`fs`, `git`), and emits a paired
`Dockerfile` + `[containers.<name>]` config block under
`.agents/outrig/containers/<name>/`. Combinations outside the menu --
Postgres dev container, embedded toolchains, internal SDKs, an MCP server
the maintainers haven't heard of -- aren't reachable from the wizard.

In practice, the project owner has had good results pointing Claude/Codex
at the public docs (`https://tgockel.github.io/outrig/`) and asking it to
produce a Dockerfile + config pair. That works because the docs are
self-contained and the constraints are well-described. It just isn't a
first-class workflow -- the user has to find the docs URL, paste the right
pages in, and the AI has no way to validate its output before declaring
done.

The goal is to give any AI tool a single, official entry point that closes
that loop: the binary itself serves the docs, the schema, the template
fragments, and validation -- so the AI can iterate against ground truth
instead of guessing.

Existing surfaces this builds on:

- `src/bin/outrig.rs:88-95` -- clap subcommand wiring.
- `src/container/add.rs:85-107` -- the preset lists (base images,
  toolchains, MCP server presets) the existing wizard offers.
- `src/container/render.rs:151-245` -- the `include_str!` Dockerfile
  fragments the wizard composes; today they're addressed by inline
  match-arms, not by name.
- `src/config/mod.rs:23-193` -- `ContainerConfig`, `McpServerSpec`, and
  the rest of the typed schema; the source of truth for "what's a valid
  container-config".
- `src/mcp.rs` -- the existing `rmcp` *client* integration; the server
  side ships with the same crate and follows symmetric patterns.
- `plan/todo/0035-0041` -- the in-flight `outrig mcp` subcommand that
  serves session-scoped MCP from the host. The design subcommand is a
  peer of that one, not a fork of it.
- `doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`,
  `doc/concepts/workspace.md`, `doc/reference/config.md`,
  `doc/usage/container.md` -- the doc pages an AI needs to consume to do
  the job end-to-end.

## Goal

`outrig mcp design` runs an MCP server on the host (no container -- this
is a tool-design assistant, not a session MCP) over stdio. An AI tool
attaches to it the same way it attaches to any other MCP server. The
agent uses the server's tools to read docs, introspect the config schema,
list and read template fragments, and validate proposed
Dockerfile / config pairs. The AI proposes files; the user installs them.

`outrig design prompt` is a parallel, dependency-free fallback that
prints a self-contained prompt to stdout for users without MCP-capable
tools.

## Why an MCP server, not just a prompt

Three of the four design options were considered:

1. **Pre-baked prompt in docs.** Cheapest, but the prompt drifts from the
   binary as features land, and the AI can't validate its output -- it
   produces something that *looks* right and the user finds out at build
   time.
2. **`outrig mcp design`** -- the recommended approach. The binary is the
   source of truth; new base images, schema fields, and MCP presets show
   up automatically. The agent gets a closed feedback loop:
   `validate_config` and `validate_dockerfile` let it iterate until its
   output parses and matches conventions, instead of guessing.
3. **In-binary AI dialog inside `container add`.** Polished UX, but
   forces outrig to ship LLM client code in the setup path, choose
   defaults for model/provider, manage API keys before providers are even
   configured, and pay LLM cost on every scaffold. The user's own AI tool
   already solves all of those problems.

The MCP-server design also serves option (1) trivially: the docs and
schema the server returns are exactly what `outrig design prompt` packs
into its one-shot output. One source of truth, two delivery shapes.
Option (3) becomes a thin wrapper later if it proves valuable -- it's not
foreclosed, just deferred.

## Surface

### `outrig mcp design`

Stdio MCP server. Registered tools:

#### Reading docs

| Tool          | Purpose                                                                |
|---------------|------------------------------------------------------------------------|
| `list_docs`   | Enumerate the embedded doc pages with one-line summaries.              |
| `get_doc`     | Return the markdown of a doc page (`{ "page": "concepts/containers" }`). |

The doc set ships with the binary via `include_str!` of the `.md` files
in `doc/concepts/` and `doc/reference/config.md`, `doc/usage/container.md`.
The bundle is frozen at build time -- no runtime fetch -- so a binary's
view of "the docs" matches its features exactly.

#### Schema introspection

| Tool                | Purpose                                                                              |
|---------------------|--------------------------------------------------------------------------------------|
| `get_config_schema` | Return JSON Schema for `ContainerConfig` and `McpServerSpec`, derived from the same `serde` types the loader parses (via `schemars`). |

A pure projection of the in-tree types. If the schema and the loader ever
disagree, that's a bug in `schemars`/derive, not a sync problem.

#### Template fragments

| Tool                       | Purpose                                                                  |
|----------------------------|--------------------------------------------------------------------------|
| `list_template_fragments`  | Enumerate the named fragments `outrig container add` ships (e.g. `header.debian`, `header.alpine`, `rust.debian`, `node.debian`, `python.debian`, `go.debian`, `mcp.fs`, `mcp.git`) with what each contributes. |
| `get_template_fragment`    | Return the literal Dockerfile snippet for a named fragment.              |

Today the fragments live in `src/container/render.rs` and are looked up
via inline match-arms during render. This task refactors them into a
named registry (`fn fragments() -> &'static BTreeMap<&str, Fragment>` or
similar) so the design tools and the wizard read from the same source.
The wizard's behavior is unchanged; the tools now expose what the wizard
already knew.

#### Preset metadata

| Tool                 | Purpose                                                                       |
|----------------------|-------------------------------------------------------------------------------|
| `list_base_images`   | Return the curated base images with reasoning (when to pick which).           |
| `list_mcp_presets`   | Enumerate the built-in MCP server presets (`fs`, `git`, ...) with their command form, install steps, and any host env requirements (e.g. `${GITHUB_TOKEN}`). |

These power the agent's "what's already known to work" awareness. They
read from the same constants `outrig container add` reads from
(`src/container/add.rs:85-107`), so the agent and the wizard agree on
what's curated.

#### Validation

| Tool                    | Purpose                                                                            |
|-------------------------|------------------------------------------------------------------------------------|
| `validate_dockerfile`   | Apply the conventions check (`CMD ["sleep", "infinity"]`, no `USER`, presence of a package providing `useradd`/`groupadd`, MCP server binaries installed for any servers the paired config references). Return structured warnings/errors. |
| `validate_config`       | Parse a TOML fragment as a `[containers.<name>]` block; return parse errors and schema violations from the existing config loader. |

`validate_dockerfile` and `validate_config` are kept as separate tools so
the agent can validate incrementally (it'll often have a config block
ready before the Dockerfile, or vice versa). A future `validate_pair`
helper that calls both and additionally cross-checks "every MCP server
named in the config has its binary installed in the Dockerfile" is a
natural follow-up; not v0.

#### Out of scope (deliberately)

- **No write tools.** The server *cannot* create files in the user's
  repo, run `outrig container add`, mutate `config.toml`, or trigger
  `podman build`. The agent proposes; the user installs (by running
  `outrig container add` themselves with the proposed inputs, or by
  pasting the files in). This containment is what lets a user safely
  attach `outrig mcp design` to *any* AI without trust assumptions.
- **No `try_build`.** A "do a real build and tell me what failed" tool is
  appealing but expensive (60-120 s per AI iteration). Punted to a
  follow-up; if it lands, gate behind an explicit cap
  (`--max-build-attempts=1`).
- **Topics other than containers.** The server is scoped to
  container-config design. A future `outrig mcp design --topic agents`
  could cover agent / provider design separately.

### `outrig design prompt`

Subcommand that prints a self-contained prompt to stdout. Composition:

1. A short system message: "you are designing a container-config for
   outrig version X.Y.Z; here are the constraints and conventions; here
   is the schema; here are example pairs."
2. The same doc bundle the MCP server serves, concatenated.
3. A few worked examples (a Rust container, a Node container, a multi-MCP
   container).

`outrig design prompt --print-mcp-config` instead prints a copy-pasteable
JSON snippet for the popular AI tools' MCP config:

- Claude Code: the `claude mcp add outrig-design ...` shell line.
- Claude Desktop: the `mcpServers` JSON block for
  `claude_desktop_config.json`.
- Codex CLI: the equivalent block for its config.
- Cursor: the equivalent block for its MCP config.

This is the "single command" the user can hand to any agent: paste the
JSON snippet for your tool, then ask the agent to design a container.

### Documentation

A new page `doc/usage/ai-assisted-design.md`:

- **TL;DR.** "When the wizard's templates don't fit, attach
  `outrig mcp design` to your AI tool and ask it to design a container
  for you."
- **Per-tool setup.** Claude Code, Claude Desktop, Codex CLI, Cursor --
  each gets the few-line config snippet, with `outrig design prompt
  --print-mcp-config` cited as the way to regenerate them. Listed in
  alphabetical order (no playing favorites).
- **Without MCP.** `outrig design prompt | pbcopy` (or `| xclip -selection
  clipboard`); paste into ChatGPT or any chat UI.
- **What the AI sees.** A brief description of the tools the server
  exposes, so users know what their AI can actually do (and what it
  can't -- explicitly call out "the server can't write files; the AI
  proposes, you install").

Cross-links:

- `doc/usage/container.md` -- new "what if the templates don't fit"
  section pointing to the new page, near the existing `outrig container
  add` walkthrough.
- `doc/SUMMARY.md` -- new entry under "Usage".
- `doc/concepts/containers.md` and `doc/concepts/mcp-servers.md` -- "see
  also" link at the bottom.

## Architecture

### Module layout

```
src/
├── cli/
│   ├── mcp_design.rs        // new: subcommand wiring for `outrig mcp design`
│   └── design_prompt.rs     // new: subcommand wiring for `outrig design prompt`
└── mcp_design/
    ├── mod.rs               // re-exports + server bootstrap
    ├── server.rs            // stdio MCP server registration
    ├── docs.rs              // include_str! bundle + list_docs/get_doc
    ├── schema.rs            // schemars projection + get_config_schema
    ├── templates.rs         // fragment registry + list/get_template_fragment
    ├── presets.rs           // base images + MCP presets list_*
    └── validate.rs          // validate_dockerfile + validate_config
```

The `src/container/render.rs` refactor extracts the fragment registry
into a public-in-crate function so `mcp_design::templates` can read from
it. `src/container/add.rs` keeps using the same registry; behavior
unchanged.

### Schema export

Add `schemars` as a direct dep (`rmcp` may already pull it transitively;
verify and use the same version pin if so). Derive `JsonSchema` on
`ContainerConfig`, `McpServerSpec`, and any nested types.
`get_config_schema` returns the schema for `ContainerConfig` plus a
sibling `mcp_server_spec` entry for the inner-table form.

### Doc bundle

Each `.md` file is `include_str!`'d at compile time, keyed by the path
relative to `doc/` (`concepts/containers`, `reference/config`, etc.).
`list_docs` returns the keys with the first H1 + first paragraph as a
summary; `get_doc` returns the full text. A build-time check (a `const`
referencing each file) ensures the build fails if a doc page is renamed
or deleted without updating the bundle.

### Validation

`validate_config` reuses the existing config loader (`src/config/load.rs`
or wherever `Config::from_str` lives) and surfaces its parse errors
unchanged. `validate_dockerfile` is a small, conservative checker:

- Parse line-by-line. Detect `CMD` / `ENTRYPOINT`, check the final `CMD`
  is `["sleep", "infinity"]`.
- Reject any `USER` directive.
- Detect the base image (`FROM`); for known-Debian / known-Alpine bases,
  require the install line for the package providing
  `useradd`/`groupadd` (`passwd` on Debian, `shadow` on Alpine).
  Unknown bases get a softer "couldn't infer base; ensure useradd /
  groupadd are present" warning rather than an error.
- If a config TOML is supplied alongside (optional second arg), check
  that every server name in `[containers.*.mcp]` has *some* install line
  in the Dockerfile that mentions a plausible binary name. This is a
  heuristic, not a proof; flagged as a warning.

The output shape is structured (`{ errors: [...], warnings: [...] }`)
rather than free text, so the agent can branch on severity.

### MCP server registration

`server.rs` boots `rmcp`'s server-side runtime over stdio, registers
each tool with its JSON Schema, and dispatches to the per-tool modules.
Patterns mirror the existing client integration in `src/mcp.rs` --
graceful shutdown on stdin EOF, structured errors via `OutrigError`.

### Subcommand wiring

```
outrig mcp design          # runs the server (this task)
outrig mcp run [...]       # session MCP from plan/todo/0035-0041
outrig design prompt [...] # one-shot prompt printer (this task)
```

Picking `mcp design` (peer of `mcp run`) over a top-level `outrig design`
keeps stdio-MCP servers clustered under `outrig mcp`. `design prompt`
sits at the top level because it's not an MCP server -- it's a printer.

## Deliverables

- `src/cli/mcp_design.rs` and `src/cli/design_prompt.rs` -- subcommand
  wiring, registered in `src/bin/outrig.rs`.
- `src/mcp_design/` module per the layout above. Unit tests on each
  pure-function tool: schema export against a golden file,
  fragment-registry round-trip, validate-dockerfile against fixture
  Dockerfiles (good + bad), validate-config against fixture TOML
  fragments (good + bad).
- `src/container/render.rs` refactored to expose the fragment registry by
  name. `outrig container add` behavior unchanged; covered by existing
  tests.
- `Cargo.toml` -- add `schemars` if not already a direct dep.
- `doc/usage/ai-assisted-design.md` -- new page covering TL;DR, per-tool
  setup, without-MCP fallback, and what the AI sees.
- `doc/usage/container.md` -- new "what if the templates don't fit"
  cross-link section.
- `doc/SUMMARY.md` -- new entry under Usage.
- `doc/concepts/containers.md`, `doc/concepts/mcp-servers.md` -- "see
  also" links.
- `tests/mcp_design.rs` (integration) -- spawn `outrig mcp design`,
  speak MCP over stdio, exercise each tool: list_docs returns the
  expected page set; get_config_schema returns valid JSON Schema;
  validate_dockerfile flags the expected errors on a deliberately broken
  fixture; validate_config does the same.
- A small fixture set under `tests/fixtures/design/` -- one good
  Dockerfile + config pair, one Dockerfile missing `CMD sleep infinity`,
  one with a `USER` directive, one config with an invalid MCP server
  name.

## Acceptance

- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` pass.
- `outrig mcp design` boots, responds to MCP `initialize`, lists the
  expected tool set, and serves a recognizable doc page through
  `get_doc`.
- `outrig design prompt` writes a non-empty prompt to stdout and exits 0;
  `--print-mcp-config` emits valid JSON for at least the Claude Code
  config snippet.
- Manual integration: `claude mcp add outrig-design outrig mcp design`,
  prompt "design me a container for a Rust + Postgres dev environment
  with the fs MCP server and a custom MCP server that runs
  `pg-dump-mcp`". The agent calls `list_docs`, `get_config_schema`,
  `list_template_fragments`, iterates with `validate_*`, and produces a
  Dockerfile + config block that passes both validators. The user runs
  `outrig container add` (or pastes the files) and `outrig run` boots
  the agent against the new container.
- Manual no-MCP: `outrig design prompt | claude --print "design a Rust +
  Postgres container"` produces a usable single-shot answer.
- `outrig container add` (the existing wizard) is unchanged in behavior;
  its tests still pass.

## Sub-decisions

- **Subcommand naming.** `outrig mcp design` chosen as a peer of
  `outrig mcp run` (the session MCP from Phase H). Alternative
  `outrig design` (top-level) was considered and rejected -- it scatters
  stdio-MCP servers across the command tree. Bikeshed allowed.
- **`get_template_fragment` granularity.** One fragment per
  base-image-family x toolchain combination is what the wizard already
  has; expose those names directly. If finer-grained slicing is wanted
  later (e.g. "just the apt-get bits"), add a separate fragment, don't
  splinter the existing ones.
- **`try_build` (real-build validation).** Punted from v0. Useful but
  expensive; gate behind a future `--max-build-attempts=N` flag if it
  ships.
- **`validate_pair`** combining the two validators with cross-checks --
  natural follow-up; not v0. Keep validators separate first; add the
  combiner once the cross-check rules settle.
- **Doc bundle freshness.** `include_str!` from the in-tree `.md` files
  is the floor. Deferred: a build-script step that injects the version
  string and the build SHA into the bundle so the agent can self-report
  ("this is outrig 0.4.2 -- if you're seeing newer features, your binary
  is out of date"). Cheap to add later.
- **Per-tool setup snippets**: hand-written or generated? Hand-written
  for v0 (one short page is fine). If the snippet shapes drift across AI
  tools, regenerate from a small template later.
- **Where the fragment registry lives.** `src/container/render.rs`
  refactor pulls it into a public-in-crate function in the same file;
  promoting it to its own module is unnecessary unless it grows.
- **Topics beyond containers.** `outrig mcp design --topic <topic>` is
  the obvious extension point (agents, providers, MCP-server authoring).
  v0 scopes to containers; the topic flag isn't introduced yet to avoid
  shipping a degenerate single-value enum.

## Dependencies

- Soft: `plan/todo/0035-0041` (the in-flight session-MCP `outrig mcp`
  subcommand). Not a hard blocker -- this can ship before or after --
  but landing 0040 first lets the two subcommands share the
  `rmcp`-server harness, which avoids duplicating boilerplate.
- May add `schemars` as a direct dep depending on whether `rmcp` already
  pulls it; if so, no Cargo.toml change.
- The `src/container/render.rs` fragment-registry refactor is a small
  pre-requisite that this task carries; if it's done independently
  first, the rest of this task gets simpler.
