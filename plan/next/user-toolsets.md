# User toolsets: a top-level `[mcp]` library with `auto-attach`

## Context

`plan/next/user-image-library.md` gives a user somewhere to author a tool image once. It does not
give that tool a way into a session. Today the only route is a repo's own config:

```toml
# .agents/outrig/config.toml -- committed
[images.coding.mcp]
git = { sidecar = "git" }
```

That works, and stays supported, but it puts a user-local name into committed config: a
collaborator who has no `~/.outrig/images/git-tools/` gets a startup failure from a file they
never edited. It also scales badly -- a personal tool has to be named in every image-config of
every repo where it is wanted.

Two structural facts shape the fix. MCP servers are declared per image-config
(`[images.<n>].mcp`), so there is no scope in which a server can be declared once and applied
broadly. And sidecar blocks are already top-level and name-referenced, with instantiation
following reference -- `plan/done/0088-entrypoint-stdio-args.md` made that an explicit rule
precisely because "declaring is instantiating" cannot survive a global config, where a toolbox
block would otherwise start in every repo.

So the missing piece is a *named library of MCP server declarations* with an explicit opt-in
switch, at both scopes.

## Goal

Add a top-level `[mcp]` table to both config files, holding named MCP server declarations that
attach to sessions either automatically within their scope or on explicit selection.

## Deliverables

- **Top-level `[mcp]` table** in `.agents/outrig/config.toml` and `~/.outrig/config.toml`,
  entries reusing the existing `McpServerSpec` shape (`crates/outrig/src/config/mod.rs:1046`),
  including placement keys, plus two new keys: `auto-attach` (bool, default `false`) and
  `on-failure` (see fork 2).
- **Scope semantics.** `auto-attach = true` in the global config attaches that server to every
  session on the machine; in a repo config it attaches to every image-config in that repo.
  `auto-attach = false` declares the server without starting anything -- the same
  reference-drives-instantiation rule `[sidecars.<sc>]` already follows.
- **Selection surfaces** for `auto-attach = false` entries, and for overriding the default:
  - `outrig run --with <name>` and `outrig mcp --with <name>`, repeatable, for one invocation.
  - `outrig run --without <name>` to suppress an auto-attached entry a given repo does not want.
  - `[images.<n>].mcp-attach = ["git"]` -- an array of top-level `[mcp]` names -- so a repo can
    opt one image-config in without re-declaring the entry (fork 1).
- **Merge order**, extending the chain in `crates/outrig/src/container/sidecar.rs`, low to high:
  primary image `org.outrig.mcp` label -> sidecar image labels -> selected global `[mcp]` ->
  selected repo `[mcp]` -> `[images.<n>].mcp`. Whole-entry replacement keyed on server name, no
  field merging -- the rule `merge_mcp` already implements.
- **`--attach` interaction.** `outrig mcp --attach` hard-errors on any config declaring sidecars
  or placements (`crates/outrig-cli/src/cli/session_setup.rs:358-367`), because a borrowed
  container cannot own sidecars. A globally auto-attached entry with a placement would therefore
  break every `--attach` invocation in every repo. Rule: auto-attached entries are **skipped with
  a notice** under `--attach`; entries selected explicitly by `--with` or `mcp-attach` still
  error, because the user named them.
- **Provenance in `outrig mcp show-merged`.** Its per-server comment already names placement and
  origin; extend the origin vocabulary to distinguish the new sources:
  `# git: sidecar "git-tools" (global [mcp], auto-attach)`.
- **Startup visibility.** A global `auto-attach` entry adds tools to sessions in repos the user
  is not thinking about. Startup has to say which servers were attached and why. This is the same
  gap `plan/next/mcp-config-startup-visibility.md` describes -- commit `ef5b983` removed the
  "reading and merging MCP configuration" lines -- and the two are best landed together.
- **Portability diagnostic.** A repo config naming a server or image that resolves nowhere must
  say where it was looked for, including the user library path, rather than a bare not-found.
- **Docs**: `doc/reference/config.md` (the `[mcp]` schema, `auto-attach`, the merge order table),
  `doc/concepts/mcp-servers.md` (scopes and precedence), `doc/reference/cli.md` (`--with` /
  `--without`), `doc/usage/mcp.md` (`show-merged` output). The first two are **symlinks** into
  `crates/outrig-cli/src/mcp_self/docs/` -- edit the targets; `cli.md` and `mcp.md` are real
  files under `doc/`.

## Config sketch

```toml
# ~/.outrig/config.toml
[mcp]
# attaches everywhere, in every repo
git = { sidecar = "git-tools", auto-attach = true }

# declared once, pulled in per run with `--with search`
search = { image = "web-search", env = { API_KEY = "${SEARCH_API_KEY}" } }

[sidecars.git-tools]
image = "git-tools"           # a ~/.outrig/images/ library project
```

```toml
# .agents/outrig/config.toml
[mcp]
# attaches to every image-config in this repo
lint = { command = ["mcp-lint", "--stdio"], sidecar = "tools", auto-attach = true }

[images.coding]
mcp-attach = ["search"]       # this image-config also wants the user's search tool
```

```sh
$ outrig run --with search              # one-off opt-in
$ outrig run --without git              # one-off opt-out of an auto-attached entry
```

## Runtime behavior

Selection resolves before placement planning, producing the set of `[mcp]` entries in play for
this session; `plan_from_config` (`container/sidecar.rs`) then sees them as ordinary entries and
the whole downstream path -- sidecar instantiation, label merge, three-phase bring-up, teardown
-- is unchanged. A named sidecar starts only if a selected entry references it, so an unselected
entry costs nothing, exactly as an unreferenced `[sidecars.<sc>]` block does today.

`--without` removes an entry from the selected set before any container is resolved, so
suppressing an auto-attached tool also skips building or pulling its image.

Server names live in one flat per-session namespace, so a `[mcp]` entry can collide with a name
declared in an image label, in `[images.<n>].mcp`, or in the other config file. The merge order
resolves those by replacement. A collision against `crate::RESERVED_SERVER` stays an error.

## Acceptance

- A global `[mcp]` entry with `auto-attach = true` provides its tools in a repo whose config
  never mentions it, with no repo config change.
- The same entry with `auto-attach = false` provides nothing until `outrig run --with <name>`,
  and starts no container in the meantime.
- `--without <name>` suppresses an auto-attached entry and skips resolving its image.
- A repo `[mcp]` entry with `auto-attach = true` applies to every image-config in that repo and
  to no other repo.
- `[images.<n>].mcp-attach = ["git"]` attaches the named global entry to that image-config only.
- `[images.<n>].mcp` overrides a same-named `[mcp]` entry in full; repo `[mcp]` overrides global
  `[mcp]`; both override an image label.
- `outrig mcp --attach` succeeds in a repo where a global auto-attached placement entry exists,
  printing a skip notice; `outrig mcp --attach --with <that entry>` still errors.
- `outrig mcp show-merged` names the source and selection reason for every server.
- An auto-attached entry whose `${VAR}` is unset degrades per fork 2 rather than failing every
  session on the machine.
- Sessions with no `[mcp]` table anywhere behave identically to today.

## Design forks

Each item leads with its status: **Resolved** (committed here), **Recommended** (a lean a
prototype should confirm), or **Open** (deferred).

1. **How an image-config references a `[mcp]` entry -- Recommended:
   `[images.<n>].mcp-attach = [...]`.** An array of top-level entry names, parallel to the way
   `[images.<n>].mcp` entries name a `[sidecars.<sc>]` block. Rejected: re-declaring the whole
   entry under `[images.<n>].mcp`, which works today with no new key but duplicates the
   declaration and drifts. Also considered and rejected: an empty-table reference form
   (`[images.coding.mcp] git = {}`), which overloads a shape that currently means "a server with
   no command". Confirm the key name against `mcp-attach` reading as a verb where the sibling
   `mcp` reads as a noun.

2. **Failure tolerance for auto-attached entries -- Recommended: default to warn.** An `env`
   value of `"${GITHUB_TOKEN}"` fails MCP startup by design when the host var is unset, naming
   the variable, server, and key. That is right for a server the user asked for. For a globally
   auto-attached one it means an unset variable breaks *every* session in *every* repo, with the
   cause in a file the user has not opened in months. Lean: auto-attached entries default to
   `on-failure = "warn"` -- log, skip that server, continue -- while explicitly selected entries
   keep `abort`. This needs `on-failure` as a key on the `[mcp]` entry itself, because an
   anonymous `image = ...` entry has no `[sidecars.<sc>]` block to carry the existing key, and
   because the default has to differ by selection reason rather than by declaration site. Confirm
   that a warn default does not make a genuinely broken toolset invisible; the startup visibility
   deliverable is what keeps it honest.

3. **Reusing the table name `[mcp]` -- Resolved.** `[mcp]` is already the top-level table name in
   a standalone `image.toml`, where it means the same thing: a map of named server declarations.
   Using it in `config.toml` for a differently-scoped map of the same shape is consistent rather
   than colliding -- the files are distinct, and `[images.<n>].mcp` is already the same shape at
   a third scope. Rejected: `[toolsets]` or `[user-mcp]`, which name the intent rather than the
   contents and would leave three names for one concept.

4. **Mid-session attach -- Open.** The REPL has `/sidecar add` for `start = "manual"` blocks, and
   `plan/done/0081-sidecar-dynamic-add.md` deliberately gave the agent no invocable surface for
   it. A `/mcp attach <name>` over the `[mcp]` library is a natural extension but inherits that
   entry's toolset-refresh machinery and its trust decision; it belongs with whatever revisits
   `/sidecar`.

5. **Ordering within a scope -- Open.** Servers connect in `BTreeMap` name order and tools are
   prefixed `<server>__<tool>`, so ordering is observable only in startup output and log naming.
   If a scope ever needs to express priority beyond name order, that is a separate change.

## Dependencies

- **Hard: 0096** (`plan/todo/0096-config-path-provenance.md`), transitively -- a `[mcp]` entry
  that references a library image is only useful once library images resolve.
- **Soft: `plan/next/user-image-library.md`.** The `[mcp]` library is independently useful over
  existing `[sidecars.<sc>]` blocks and raw image refs, but the motivating case is a user-library
  tool. Landing the library first makes the acceptance criteria here demonstrable end to end.
- **Soft: `plan/next/mcp-config-startup-visibility.md`.** The startup-visibility deliverable is
  that entry's subject; landing them together avoids doing the same span work twice.

## See also

- `crates/outrig/src/config/mod.rs` -- `McpServerSpec` (the entry shape being reused) and
  `Config`, which gains the top-level map.
- `crates/outrig/src/container/sidecar.rs` -- `plan_from_config` and the module-level merge-order
  documentation this extends.
- `crates/outrig/src/container/embedded.rs` -- `merge_mcp` / `merge_mcp_with_source` and
  `McpDeclarationSource`, which gains variants for the new scopes.
- `crates/outrig-cli/src/cli/session_setup.rs` -- the `--attach` rejection at 358-367, and the
  three-phase sidecar bring-up that selection feeds.
- `plan/done/0088-entrypoint-stdio-args.md` -- why instantiation follows reference, the rule
  `auto-attach` has to opt out of explicitly.
- `plan/done/0079-sidecar-core-exec-stdio.md` -- the `--attach` + sidecars error and the
  `on-failure` semantics fork 2 borrows.
- `plan/done/0081-sidecar-dynamic-add.md` -- the deliberate absence of an agent-invocable add
  (fork 4).
- `plan/next/user-image-library.md` -- where the images these entries reference come from.
