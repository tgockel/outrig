# 0056 -- `outrig design prompt` -- one-shot prompt printer

## Context

`plan/todo/0055-outrig-mcp-self.md` ships the MCP-server side of
self-description -- the closed-loop, validate-as-you-iterate path for
AI tools that speak MCP. Not every user has an MCP-capable tool, and
some workflows (one-off ChatGPT chat, paste-into-a-form) don't fit the
attach-a-server model.

`outrig design prompt` covers that gap. It prints a self-contained
prompt to stdout: system message + the same doc bundle the MCP server
vends + a few worked examples. The user pipes that into any chat UI
and gets a usable answer in one round-trip, without iterative
validation.

Both this task and 0055 share the same doc bundle, so they have to
ship in order. 0055 lands first; this task pulls in the same
`include_str!` set and adds the prompt-construction wrapper around it.

## Goal

`outrig design prompt` prints a self-contained design prompt to stdout
and exits 0. `outrig design prompt --print-mcp-config <tool>` instead
prints the JSON / shell snippet to wire `outrig mcp self` into a named
AI tool's MCP config. Together they cover the "no MCP" and "wire up
MCP" paths to `outrig mcp self`.

## User surface

```bash
# Pipe to any chat UI:
outrig design prompt | pbcopy

outrig design prompt > /tmp/prompt.txt
# Open ChatGPT, paste, get a Dockerfile + config back.

# Or generate the MCP config snippet for the user's AI tool:
outrig design prompt --print-mcp-config claude-code
outrig design prompt --print-mcp-config claude-desktop
outrig design prompt --print-mcp-config codex
outrig design prompt --print-mcp-config cursor
```

`--print-mcp-config` accepts: `claude-code`, `claude-desktop`,
`codex`, `cursor`. Listed alphabetically. Unknown values: clap-time
error listing the valid set.

### Prompt composition

The default (no flag) prompt has three sections:

1. A short system message: "you are designing a container-config for
   outrig version X.Y.Z; here are the constraints and conventions;
   here is the schema; here are example pairs."
2. The same doc bundle the MCP server serves, concatenated.
3. Two or three worked examples (a Rust container, a Node container,
   a multi-MCP container).

The version number comes from `env!("CARGO_PKG_VERSION")` at compile
time; no runtime fetch.

### `--print-mcp-config` output shapes

| Tool             | Output                                                              |
|------------------|---------------------------------------------------------------------|
| `claude-code`    | Shell line: `claude mcp add outrig-self outrig mcp self`.           |
| `claude-desktop` | JSON block for `claude_desktop_config.json`'s `mcpServers` field.   |
| `codex`          | Equivalent block for the Codex CLI MCP config.                      |
| `cursor`         | Equivalent block for Cursor's MCP config.                           |

Each is a copy-pasteable snippet -- shell line for Claude Code, JSON
for the others. Hand-written templates in `src/cli/design_prompt.rs`;
the snippets are short enough that templating is overkill.

## Architecture

### Module layout

```
src/
└── cli/
    └── design_prompt.rs    // new: subcommand wiring + composer
```

The composer reuses the doc bundle from `src/mcp_self/docs.rs` (added
in 0055) -- it's already a `&'static [(&'static str, &'static str)]`
of (key, contents) tuples. The composer concatenates them in a
deterministic order, prepends the system message, and appends the
worked examples.

Worked examples: small (~20-line) Dockerfile + config pairs embedded
inline in `design_prompt.rs` as `&'static str` constants. Three of
them: Rust, Node, multi-MCP. Each labelled with what it demonstrates.

### Subcommand wiring

```
outrig design prompt              # this task
outrig mcp self                   # 0055 (sibling)
outrig mcp run [...]              # plan/done/0035-0041
```

`outrig design` is the top-level subcommand; `prompt` is its only
subsubcommand for now. The shape leaves room for future `outrig
design <other>` siblings without changing the existing surface.

## Documentation deliverables

Documentation lands with the feature.

### Fill in `doc/usage/ai-assisted-design.md`

0055 created the page with a placeholder "Without MCP" section. This
task fills it in:

- `outrig design prompt | pbcopy` (or `| xclip -selection clipboard`)
  on Linux. Paste into ChatGPT, Claude.ai web, or any chat UI.
- `outrig design prompt > prompt.txt` for users who want to inspect
  the prompt before sending it.
- Note that the MCP path (0055) is preferred when available because it
  closes the validation loop; the prompt path is a one-shot fallback.

The "Per-tool setup" section already added in 0055 gets a footnote
pointing at `outrig design prompt --print-mcp-config <tool>` as the
way to regenerate the snippets if they drift.

### `doc/SUMMARY.md`

No new entry needed -- `doc/usage/ai-assisted-design.md` was added by
0055; this task only fills in a section.

## Deliverables

- `src/cli/design_prompt.rs` -- subcommand wiring + prompt composer +
  per-tool snippet emitter.
- `src/bin/outrig.rs` -- register the new `design prompt` subcommand.
- `doc/usage/ai-assisted-design.md` -- "Without MCP" section filled
  in; cross-link added under "Per-tool setup".
- `tests/design_prompt.rs` (integration) -- run `outrig design
  prompt`, assert non-empty stdout containing the version string and
  recognizable doc-bundle markers; run `outrig design prompt
  --print-mcp-config <tool>` for each supported tool, assert valid
  JSON (where applicable) and recognizable shape.

## Acceptance

- `outrig design prompt` writes a non-empty prompt to stdout and exits
  0. Output contains the OutRig version, the bundled doc set, and at
  least one worked example.
- `outrig design prompt --print-mcp-config claude-code` prints a shell
  line containing `outrig mcp self`.
- `outrig design prompt --print-mcp-config claude-desktop` prints valid
  JSON parseable by a JSON parser, with an `mcpServers` field
  referencing `outrig mcp self`.
- `outrig design prompt --print-mcp-config codex` and `cursor` likewise
  print valid copy-pasteable snippets.
- `outrig design prompt --print-mcp-config bogus` exits non-zero with
  a clap error listing the valid tool names.
- Manual: `outrig design prompt | claude --print "design a Rust +
  Postgres container"` produces a usable single-shot answer.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`
  pass.

## Sub-decisions

- **Why a separate subcommand instead of a flag on `outrig mcp self`?**
  `outrig mcp self` runs an MCP server -- it doesn't print to stdout
  in a way that fits a pipe. The two surfaces are mechanically
  different.
- **Worked examples count.** Three (Rust, Node, multi-MCP). Fewer
  loses coverage; more pads the prompt without obvious gain. The
  number is debatable; revisit if a real prompt needs different
  examples.
- **`--print-mcp-config` output format detection.** No auto-detect;
  the user names their tool. Auto-detect would either be wrong (env
  vars overlap across tools) or require a query that defeats the "one
  shot" framing.
- **Shipping the per-tool snippets in the binary vs. fetching them.**
  Embed them. Cheap to maintain; lets the binary work offline; users
  who want fresh snippets re-pull the binary.

## Decisions

- Reuse the 0055 doc bundle by making `src/mcp_self/docs.rs`
  crate-visible instead of duplicating the `include_str!` list in the
  design-prompt module.
- Emit Codex setup as TOML, matching the existing
  `doc/usage/ai-assisted-design.md` snippet. Claude Desktop and Cursor
  remain JSON snippets.
- Treat the quickstart's "AI-guided init" note as documentation for the
  post-`outrig init` refinement path, not as a new `outrig init` flag.

## Dependencies

- **Hard: 0055** (`outrig mcp self`). Reuses the doc bundle from
  `src/mcp_self/docs.rs`; the `--print-mcp-config` snippets reference
  the `outrig mcp self` subcommand; the doc page filled in here was
  created in 0055.
