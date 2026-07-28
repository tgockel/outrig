# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

This queue is pre-freeze work for `0.2.0`. Everything in it either changes what an existing
thing means or fixes a contract a release would otherwise freeze wrong. The public surface is
now settled: 0094 sealed every public type with `#[non_exhaustive]` and shipped the
constructors that replace the struct literals it forbids, which is what makes 0097-0101
additive rather than breaking -- each of those adds a field or a variant to a type the sweep
insulates. 0095 finished the other half -- the options struct, the sealed `BackingClient`, and
an opaque `ImageTag`, the last changes that are themselves breaking -- leaving 0096 as the only
queued item whose own change breaks the surface. 0096 is also the one item an external embedder
blocks on, so it wants to land in the published 0.2.0 rather than a follow-on. The e2e suite
compiles again and CI gates it (0092), so `library_surface.rs` -- the facade test the work is
measured against -- is runnable. 0093 settled the reach question: all six of the library's
public modules are supported API, so 0094-0095 cover more than first scoped, and
`outrig-cli`'s internals are no longer public at all.

| Task | Title                                                     | Dependencies     |
|------|-----------------------------------------------------------|------------------|
| 0096 | Library parity for sidecar placements and primary exec    | --               |
| 0097 | Resolve relative config paths against the declaring file  | 0094             |
| 0098 | Native Anthropic Messages API provider                    | 0094             |
| 0099 | Make subagent release atomic rather than partial          | 0092             |
| 0100 | Cap how many subagents run at once                        | 0099             |
| 0101 | Launch a subagent under a different model                 | 0093, 0098, 0100 |
