# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

This queue is pre-freeze work for `0.2.0`. Everything in it either changes what an existing
thing means or fixes a contract a release would otherwise freeze wrong. The public surface is
now settled and, with 0096 landed, every breaking change it will take is in: 0094 sealed every
public type with `#[non_exhaustive]` and shipped the constructors that replace the struct
literals it forbids, which is what makes 0098-0101 additive rather than breaking -- each of
those adds a field or a variant to a type the sweep insulates. 0097 was the first to cash that
in, adding a `ConfigSource` to `ImageConfig` / `MountConfig` and a `declared_in` field to two
sealed `ConfigValidationError` variants without a single breaking change. 0095 finished the
other half --
the options struct, the sealed `BackingClient`, and an opaque `ImageTag`. 0096 reshaped
`SidecarServerSpec` into an enum and gave the library API every sidecar placement the config
path has, which is what an external embedder was blocked on. The e2e suite compiles again and
CI gates it (0092), so `library_surface.rs` -- the facade test that work was measured against
-- is runnable. 0093 settled the reach question: all six of the library's public modules are
supported API, so 0094-0095 cover more than first scoped, and `outrig-cli`'s internals are no
longer public at all. 0098 cashed the sweep in a second time, adding an `LlmProvider::Anthropic`
variant and a `Model::max_tokens` field additively; the one break it did take -- renaming three
provider-specific `ConfigValidationError` variants -- was chosen, not forced.

Everything through 0106 landed; 0105-0111 refilled the queue from `plan/next/`
against one gate: **does deferring this force a breaking change later, or freeze a contract a
point release cannot fix?** The 0094 sweep is what makes that question narrow -- `#[non_exhaustive]`
makes field and variant *additions* free, so what is left are field *type* changes, variants the
sweep did not seal, and published method signatures. Seven of the 39 entries in `plan/next/`
qualified. The rest are additive by construction, internal, or explicitly post-v0, and stay there.

- **0107 [exec-workdir](0107-exec-workdir.md)** -- four published methods take
  `(&[String], &BTreeMap)` today: `exec_stdio` and `exec_capture`, on both `Outrig` and
  `Container`.
- **0108 [global-workspace-block-dropped](0108-global-workspace-block-dropped.md)** -- a global
  `[workspace]` block is parsed, validated, merged, and discarded; honoring it and rejecting it
  both change what a config `0.2.0` accepts does.
- **0109 [subagent-tree-shutdown-grace](0109-subagent-tree-shutdown-grace.md)** -- the five-second
  teardown budget was never measured against the 72-subagent tree the width cap permits, and
  `DEFAULT_SUBAGENT_WIDTH_MAX` is a `pub const`.
- **0110 [model-aliases](0110-model-aliases.md)** -- `Model::provider` becomes `Option<String>`,
  a field *type* change the sweep does not cover.
- **0111 [provider-construction-options-struct](0111-provider-construction-options-struct.md)** --
  `LlmProvider::openai` / `::anthropic` take their fields positionally, and Rust cannot overload.

0105 landed, and was the cheapest and highest-value of them: 0097 had already built the machinery
-- `ConfigSource`, per-mount stamping, `resolved_host_path()` -- and given `declared_in` to the two
image variants it could reach additively, so threading provenance through the mount variants was
mostly wiring. A mount diagnostic now ends with `(declared in "<file>")`, which is what the
concatenated global+repo mount list makes necessary: a bare relative path is ambiguous between two
files. All five `MountRuleViolation` variants and all five `WorkspaceMount*` variants took it and
are now `#[non_exhaustive]`, so the next field is additive. Its one open fork was resolved *yes*:
the container-path rules carry it too, `ContainerRoot` ceasing to be a unit variant, because the
clause answers which file to go edit rather than what a path resolved against -- and that rule's
message carries no path at all, so the declaring file is the only handle it has.

0111 is last deliberately. It is the only one of the seven that is API *shape* rather than API
*correctness*: rc.1 shipped `with_retry_budget_secs` as the non-breaking workaround, and
`#[non_exhaustive]` means downstream cannot construct the variants anyway. It is the first entry to
cut if the window tightens.

Two things are **not** queued and are worth stating so they are not re-litigated. Regenerating
`crates/outrig/public-api.txt` is each task's own deliverable rather than a task of its own -- four
of the seven rewrite it, and a queued step would only be a second place to forget.
`plan/next/public-api-snapshot-gate.md` (an opt-in check so the snapshot cannot silently rot) stays
in the buffer as post-release process hygiene; a check against `cargo-public-api` 0.52.0 found no
semantic drift in the current file, only `std::io` -> `core::io` renderings.

0100 landed: `subagent-width-max` (default `8`, range `1..=16`) bounds how many live subagents one
launching agent may hold, refusing past the limit rather than queueing. It was sequenced before
0101 deliberately -- model selection makes wide fan-out expensive rather than merely slow, so the
containment half belonged first, and 0101 could then assume a bounded tree. What the cap does not
settle is whether the five-second teardown budget fits the tree it permits; that measurement is
filed as `plan/next/subagent-tree-shutdown-grace.md`.

0101 landed, and closed the arc: `outrig__subagent` takes an optional `model` naming a
`[models.<name>]`, so an expensive parent can farm mechanical work out to a cheap subagent instead
of running the whole session cheap. Omitting it inherits the launching agent's model and is
byte-for-byte the old path, in behavior and in the logs. The schema's `enum` lists only the models
the running build can actually reach, and disappears entirely when that set holds one name.
Sampling and limits stay with the launching agent -- carried forward by overwrite direction, which
is what keeps the session's `--max-tool-calls` / `--max-tool-result-bytes` alive across a
re-resolution. Three of its forks were left open on purpose: the device override and per-model
sampling defaults have no pressing audience, and the operator allowlist -- the one that matters,
since an agent can escalate *itself* onto the expensive model -- is filed as
`plan/next/subagent-model-allowlist.md`.

0102-0104 were one arc, and the last of them is why the first two were pre-freeze work rather
than polish. This repo's `.agents/outrig/config.toml` used to embed every MCP server in the
primary image, so nothing in daily use exercised sidecar placement, `view = "primary"`, or the
`image-name` pull path -- the feature only ran under the gated e2e suite. Moving the three
servers out found three defects that a release would otherwise have frozen: a `view = "primary"`
payload ran as root in the primary's user namespace and left subuid-owned files behind (0102,
landed -- the launcher now drops to the session's ids once the graft is in place), the
launcher's missing `PATH` lookup made the published quickstart image unusable, which is what
kept `primary_view_e2e.rs` red on trunk (0103, landed -- a bare program name is resolved
against the sidecar image's `PATH` before the namespace join, and that e2e now passes against
the stock image), and the payload inherited the primary's procfs, in which `/proc/self` names
nothing, so every rustup shim failed (0104, landed -- the launcher mounts a fresh `proc`). The
arc is closed; all three servers now run as `view = "primary"` sidecars every session.

0106 landed: `request-timeout-secs` is bounded to `1..=3600`, the hour its sibling
`retry-budget-secs` already used. Two things it turned up are worth recording, because both were
assumptions the queue carried rather than facts. The key has **no top-level form** -- it lives
only on the two remote provider variants, so the task's "top level and per provider" wording
described a path that does not exist; the criteria were corrected to the one that does, and the
missing default is filed as `plan/next/top-level-request-timeout-secs.md`, additive and so not
window-bound. And `0` is an *immediate* timeout in reqwest rather than a disabled one, the
opposite of what the `plan/next/` entry it was groomed from claimed -- verified against the
pinned 0.13.4 source, not inferred. That makes `0` mean opposite things on the two keys, which
is correct: the budget counts attempts, the timeout bounds one. A third finding was incidental --
`style = "mistralrs"` is a unit variant, so `deny_unknown_fields` silently swallows *every*
unknown key on it, filed as `plan/next/mistralrs-provider-swallows-keys.md` and pinned by a test
written against today's behavior.
