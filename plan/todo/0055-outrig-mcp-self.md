# 0055 -- `outrig mcp self` -- self-description MCP server

## Context

`outrig container add` is template-driven. It walks the user through a
fixed menu of base images, language toolchains, and built-in MCP server
presets, and emits a paired `Dockerfile` + `[containers.<name>]` config
block under `.agents/outrig/containers/<name>/`. Combinations outside
the menu -- Postgres dev container, embedded toolchains, internal SDKs,
an MCP server the maintainers haven't heard of -- aren't reachable from
the wizard.

In practice, pointing an AI tool at the public docs and asking it to
produce the pair works. It works because the docs are self-contained
and the constraints are well-described. It just isn't a first-class
workflow -- the user has to find the docs URL, paste the right pages
in, and the AI has no way to validate its output before declaring
done.

Goal: give any AI tool a single, official entry point that closes that
loop. The binary itself serves the docs, the schema, and validation,
so the AI can iterate against ground truth instead of guessing.

The framing is **self-description**, not "design assistant". The MCP
server vends what OutRig knows about itself; container-config design
is the dominant *use case*, but the tools are general-purpose
self-introspection. The paired one-shot prompt shipper (`outrig design
prompt`) lives in a sibling task (0056).

Existing surfaces this builds on:

- `src/bin/outrig.rs:88-95` -- clap subcommand wiring.
- `src/config/mod.rs:23-193` -- `ContainerConfig`, `McpServerSpec`,
  and the rest of the typed schema; the source of truth for "what's a
  valid container-config".
- `src/mcp.rs` -- the existing `rmcp` *client* integration; the server
  side ships with the same crate and was reused by `plan/done/0040`.
- `plan/done/0035-0041` -- the session-MCP `outrig mcp` subcommand.
  This task adds a new sibling `self` subsubcommand and reuses the
  same rmcp-server harness.
- `doc/concepts/containers.md`, `doc/concepts/mcp-servers.md`,
  `doc/concepts/workspace.md`, `doc/reference/config.md`,
  `doc/usage/container.md` -- the doc pages an AI consumes to do the
  job end-to-end.

## Goal

`outrig mcp self` runs an MCP server on the host (no container -- this
is self-introspection, not session MCP) over stdio. An AI tool attaches
to it the same way it attaches to any other MCP server. The agent uses
the server's tools to read docs, introspect the config schema, see
suggested base images and MCP presets, and validate proposed
Dockerfile / config pairs. The AI proposes files; the user installs
them.

## User surface

Stdio MCP server. Registered tools:

### Reading docs

| Tool        | Purpose                                                                |
|-------------|------------------------------------------------------------------------|
| `list_docs` | Enumerate the embedded doc pages with one-line summaries.              |
| `get_doc`   | Return the markdown of a doc page (`{ "page": "concepts/containers" }`). |

The doc set ships with the binary via `include_str!` of the `.md`
files in `doc/concepts/` and `doc/reference/config.md`,
`doc/usage/container.md`, and the new `doc/concepts/mcp-trust-model.md`
delivered with this task. The bundle is frozen at build time -- no
runtime fetch -- so a binary's view of "the docs" matches its features
exactly.

### Schema introspection

| Tool                | Purpose                                                                              |
|---------------------|--------------------------------------------------------------------------------------|
| `get_config_schema` | Return JSON Schema for `ContainerConfig` and `McpServerSpec`, derived from the same `serde` types the loader parses (via `schemars`). The response also includes a `paths` block describing where each file lives: top-level `outrig.toml` (project root), per-container Dockerfile / context (`.agents/outrig/containers/<name>/`), and image-side `container.toml` once 0053 lands (`/etc/outrig/container.toml`). |

A pure projection of the in-tree types. If the schema and the loader
ever disagree, that's a bug in `schemars`/derive, not a sync problem.

### Preset suggestions

| Tool                | Purpose                                                                       |
|---------------------|-------------------------------------------------------------------------------|
| `list_base_images`  | Return curated base images with reasoning (when to pick which). **Framed as suggestions only**: the response carries an explicit "the LLM may pick any base image; this list is what `outrig container add` would offer" disclaimer field. |
| `list_mcp_presets`  | Enumerate built-in MCP server presets (`fs`, `git`, ...) with their command form, install steps, and any host env requirements. **Framed as suggestions only**: not an exhaustive registry; new MCP servers ship constantly and the list will be incomplete. |

The "suggestions only" framing is in the *response shape*, not just the
docs. Each list returns an object like `{ "note": "suggestions only --
not exhaustive; pick anything that fits", "items": [...] }` so the LLM
sees the disclaimer at use time. Reuse the constants
`outrig container add` reads from (`src/container/add.rs:85-107`) so
the suggestion list and the wizard agree on what's curated.

**No template-fragment tools.** The original spec proposed
`list_template_fragments` / `get_template_fragment` returning the
in-tree Dockerfile snippets. Dropped: an LLM with web search can find
current images and packages, baked-in fragments rot fast (filesystem
MCPs alone change frequently), and the schema + suggestion lists are
enough scaffolding for the LLM to produce its own fragments. The
fragment-registry refactor of `src/container/render.rs` is not
required; `outrig container add` keeps using its inline match arms.

### Validation

| Tool                  | Purpose                                                                            |
|-----------------------|------------------------------------------------------------------------------------|
| `validate_dockerfile` | Apply the conventions check (`CMD ["sleep", "infinity"]`, no `USER`, presence of a package providing `useradd`/`groupadd`, MCP server binaries installed for any servers the paired config references). Return structured warnings/errors. |
| `validate_config`     | Parse a TOML fragment as a `[containers.<name>]` block; return parse errors and schema violations from the existing config loader. |

Kept as separate tools so the agent can validate incrementally (it'll
often have a config block ready before the Dockerfile, or vice versa).
A future `validate_pair` helper that calls both and additionally
cross-checks "every MCP server named in the config has its binary
installed in the Dockerfile" is a natural follow-up; not v0.

### Out of scope (deliberately)

- **No write tools.** The server cannot create files in the user's
  repo, run `outrig container add`, mutate `outrig.toml`, or trigger
  `podman build`. The agent proposes; the user installs (by running
  `outrig container add` themselves with the proposed inputs, or by
  pasting the files in). This containment is what lets a user safely
  attach `outrig mcp self` to *any* AI without trust assumptions.
- **No `try_build`.** A "do a real build and tell me what failed" tool
  is appealing but expensive (60-120 s per AI iteration). Punted to a
  follow-up; if it lands, gate behind an explicit cap
  (`--max-build-attempts=1`).
- **Topics other than containers.** The server is scoped to
  container-config self-description today. A future
  `outrig mcp self --topic agents` could cover agent / provider
  self-description separately; the topic flag isn't introduced yet to
  avoid shipping a degenerate single-value enum.

## Architecture

### Module layout

```
src/
├── cli/
│   └── mcp_self.rs         // new: subcommand wiring for `outrig mcp self`
└── mcp_self/
    ├── mod.rs              // re-exports + server bootstrap
    ├── server.rs           // stdio MCP server registration
    ├── docs.rs             // include_str! bundle + list_docs/get_doc
    ├── schema.rs           // schemars projection + get_config_schema
    ├── presets.rs          // base images + MCP presets list_*
    └── validate.rs         // validate_dockerfile + validate_config
```

### Schema export

Add `schemars` as a direct dep (`rmcp` may already pull it
transitively; verify and use the same version pin if so). Derive
`JsonSchema` on `ContainerConfig`, `McpServerSpec`, and any nested
types. `get_config_schema` returns the schema for `ContainerConfig`
plus a sibling `mcp_server_spec` entry for the inner-table form, plus
a `paths` description.

### Doc bundle

Each `.md` file is `include_str!`'d at compile time, keyed by the path
relative to `doc/` (`concepts/containers`, `reference/config`,
`concepts/mcp-trust-model`, etc.). `list_docs` returns the keys with
the first H1 + first paragraph as a summary; `get_doc` returns the
full text. A build-time `const` referencing each file ensures the
build fails if a doc page is renamed or deleted without updating the
bundle.

### Validation

`validate_config` reuses the existing config loader and surfaces its
parse errors unchanged. `validate_dockerfile` is a small, conservative
checker:

- Parse line-by-line. Detect `CMD` / `ENTRYPOINT`, check the final
  `CMD` is `["sleep", "infinity"]`.
- Reject any `USER` directive.
- Detect the base image (`FROM`); for known-Debian / known-Alpine
  bases, require the install line for the package providing
  `useradd`/`groupadd` (`passwd` on Debian, `shadow` on Alpine).
  Unknown bases get a softer "couldn't infer base; ensure useradd /
  groupadd are present" warning rather than an error.
- If a config TOML is supplied alongside (optional second arg), check
  that every server name in `[containers.*.mcp]` has *some* install
  line in the Dockerfile that mentions a plausible binary name. This
  is a heuristic, not a proof; flagged as a warning.

The output shape is structured (`{ errors: [...], warnings: [...] }`)
rather than free text, so the agent can branch on severity.

### MCP server registration

`server.rs` boots `rmcp`'s server-side runtime over stdio, registers
each tool with its JSON Schema, and dispatches to the per-tool
modules. Patterns mirror the existing client integration in
`src/mcp.rs` and the server-side harness from `plan/done/0040` --
graceful shutdown on stdin EOF, structured errors via `OutrigError`.

### Subcommand wiring

```
outrig mcp self              # this task's stdio MCP server
outrig mcp run [...]         # session MCP from plan/done/0035-0041
outrig design prompt [...]   # paired one-shot prompt printer (0056)
```

Picking `mcp self` (peer of `mcp run`) keeps stdio-MCP servers
clustered under `outrig mcp`. `design prompt` (in 0056) sits at the
top level because it's not an MCP server -- it's a printer.

## Documentation deliverables

Documentation lands with the feature, not in a follow-up.

### New page -- `doc/concepts/mcp-trust-model.md`

The container is the trust boundary. Inside the container, MCP servers
don't need to be sandboxed at the application layer:

- Filesystem servers can be pointed at `/`. The container's filesystem
  is isolated; "the LLM can read everything" means "everything in the
  container," which is what we want.
- Shell servers don't need command allowlists. Arbitrary code
  execution inside the container is exactly the point of running an
  agent in a container.
- Network servers are bounded by the container's network namespace
  (and, once the network interceptor lands, by host policy).

Implication for users: configure MCPs liberally inside the container
-- the value of OutRig is that you *can*. The page lays this out so an
LLM reading via `get_doc` knows it's allowed to propose, e.g.,
`mcp-server-filesystem /` rather than tying the server down to a
narrow workspace path.

Cross-link from `doc/concepts/mcp-servers.md` and
`doc/concepts/containers.md`. Add to `doc/SUMMARY.md` under "Concepts".

### New page -- `doc/usage/ai-assisted-design.md`

This task owns the `outrig mcp self` half:

- TL;DR: "When the wizard's templates don't fit, attach
  `outrig mcp self` to your AI tool and ask it to design a container
  for you."
- Per-tool setup. Claude Code, Claude Desktop, Codex CLI, Cursor --
  each gets a few-line config snippet. Listed alphabetically (no
  playing favorites).
- "What the AI sees." Brief description of the tools the server
  exposes, so users know what their AI can actually do (and what it
  can't -- explicitly call out "the server can't write files; the AI
  proposes, you install").
- Cross-references to the new trust-model page so the LLM and the user
  both understand "configure MCPs liberally" is the intended posture.
- A placeholder section "Without MCP" -- pointer left for 0056 to fill
  in when `outrig design prompt` lands.

### Cross-link updates

- `doc/usage/container.md` -- new "what if the templates don't fit?"
  section linking to the new page.
- `doc/SUMMARY.md` -- new entries for both new pages.
- `doc/concepts/containers.md` -- "see also" link.
- `doc/concepts/mcp-servers.md` -- "see also" link to trust-model
  page; one-line cross-reference to the design page.

## Deliverables

- `src/cli/mcp_self.rs` -- subcommand wiring, registered in
  `src/bin/outrig.rs`.
- `src/mcp_self/` module per the layout above. Unit tests on each
  pure-function tool: schema export against a golden file,
  validate-dockerfile against fixture Dockerfiles (good + bad),
  validate-config against fixture TOML fragments (good + bad).
- `Cargo.toml` -- add `schemars` if not already a direct dep.
- `doc/concepts/mcp-trust-model.md` -- new concept page.
- `doc/usage/ai-assisted-design.md` -- new usage page (the
  `outrig mcp self` half; 0056 fills in the no-MCP fallback section).
- `doc/usage/container.md` -- new "what if the templates don't fit"
  cross-link section.
- `doc/SUMMARY.md` -- new entries for both new pages.
- `doc/concepts/containers.md`, `doc/concepts/mcp-servers.md` -- "see
  also" links.
- `tests/mcp_self.rs` (integration) -- spawn `outrig mcp self`, speak
  MCP over stdio, exercise each tool: `list_docs` returns the expected
  page set including `concepts/mcp-trust-model`;
  `get_config_schema` returns valid JSON Schema with the `paths`
  block; `list_base_images` and `list_mcp_presets` responses include
  the suggestions-only disclaimer; `validate_dockerfile` flags the
  expected errors on a deliberately broken fixture; `validate_config`
  does the same.
- A small fixture set under `tests/fixtures/self/` -- one good
  Dockerfile + config pair, one Dockerfile missing
  `CMD sleep infinity`, one with a `USER` directive, one config with
  an invalid MCP server name.

## Acceptance

- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`
  pass.
- `outrig mcp self` boots, responds to MCP `initialize`, lists the
  expected tool set, and serves a recognizable doc page through
  `get_doc`.
- `list_base_images` and `list_mcp_presets` responses include an
  explicit suggestions-only disclaimer field.
- `list_docs` includes `concepts/mcp-trust-model`; `get_doc` returns
  its content.
- Manual integration: `claude mcp add outrig-self outrig mcp self`,
  prompt "design me a container for a Rust + Postgres dev environment
  with the fs MCP server pointed at /workspace and a custom MCP
  server that runs `pg-dump-mcp`". The agent calls `list_docs`,
  `get_config_schema`, `list_base_images`, `list_mcp_presets`
  (treating the latter two as suggestions only), iterates with
  `validate_*`, and produces a Dockerfile + config block that passes
  both validators. The user runs `outrig container add` (or pastes
  the files) and `outrig run` boots the agent against the new
  container.
- `outrig container add` (the existing wizard) is unchanged in
  behavior; its tests still pass.

## Sub-decisions

- **Subcommand naming.** `outrig mcp self` chosen as a peer of
  `outrig mcp run`. Alternative `outrig design` (top-level) was
  considered and rejected -- it scatters stdio-MCP servers across the
  command tree, and the framing is broader than container design.
- **Suggestions-only framing**: in the *response shape*, not just the
  docs. The original spec relied on docs alone, but the LLM may not
  have read the docs before calling a list tool. Putting the
  disclaimer in the response itself ensures it's visible at use time.
- **`try_build`** -- punted from v0. Useful but expensive; gate behind
  a future `--max-build-attempts=N` flag if it ships.
- **`validate_pair`** combining the two validators with cross-checks
  -- natural follow-up; not v0. Keep validators separate first; add
  the combiner once the cross-check rules settle.
- **Doc bundle freshness.** `include_str!` from the in-tree `.md`
  files is the floor. Deferred: a build-script step that injects the
  version string and the build SHA into the bundle so the agent can
  self-report ("this is outrig 0.4.2 -- if you're seeing newer
  features, your binary is out of date"). Cheap to add later.
- **Per-tool setup snippets**: hand-written or generated? Hand-written
  for v0 (one short page is fine). If the snippet shapes drift across
  AI tools, regenerate from a small template later.
- **Topics beyond containers.** `outrig mcp self --topic <topic>` is
  the obvious extension point (agents, providers, MCP-server
  authoring). v0 scopes to containers; the topic flag isn't
  introduced yet to avoid shipping a degenerate single-value enum.

## Dependencies

- Soft: `plan/done/0035-0041` -- the session-MCP `outrig mcp`
  subcommand shipped there. The new `self` subsubcommand reuses the
  rmcp-server harness introduced by `plan/done/0040`.
- May add `schemars` as a direct dep depending on whether `rmcp`
  already pulls it; if so, no `Cargo.toml` change.
