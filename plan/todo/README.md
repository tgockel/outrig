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

| Task | Title                                                     | Dependencies     |
|------|-----------------------------------------------------------|------------------|
| 0100 | Cap how many subagents run at once                        | --               |
| 0101 | Launch a subagent under a different model                 | 0093, 0098, 0100 |

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
