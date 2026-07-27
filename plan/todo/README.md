# Plan: v0 task queue

The per-step index of `plan/todo/`. Tasks land lowest-numbered first; every task depends only
on smaller-numbered predecessors. Run `/next-task` to advance one task end-to-end on its own
branch; run `/groom-plan` to maintain ordering after edits or `plan/next/` pulls.

See [`.claude/CLAUDE.md`](../../.claude/CLAUDE.md) for the workflow conventions.

This queue is pre-freeze work for `0.2.0`. Everything in it either changes what an existing
thing means or fixes a contract a release would otherwise freeze wrong. 0092 restores honest
acceptance testing -- the e2e suite does not compile today, and it contains the library-facade
test the hardening work is measured against. 0093-0095 settle the public surface: there is
currently no `#[non_exhaustive]` anywhere in either crate, so every additive change to a public
type is a breaking one. Landing them first is what makes 0096-0100 additive rather than
breaking -- each of those adds a field or a variant to a type the sweep insulates.

| Task | Title                                                     | Dependencies     |
|------|-----------------------------------------------------------|------------------|
| 0092 | Fix e2e-gated test bit-rot and compile the e2e suite in CI | none             |
| 0093 | Shrink the reachable public surface                       | 0092             |
| 0094 | `#[non_exhaustive]` sweep on what stays public            | 0093             |
| 0095 | Options structs, trait sealing, ImageTag privatization    | 0093, 0094       |
| 0096 | Resolve relative config paths against the declaring file  | 0094             |
| 0097 | Native Anthropic Messages API provider                    | 0094             |
| 0098 | Make subagent release atomic rather than partial          | 0092             |
| 0099 | Cap how many subagents run at once                        | 0098             |
| 0100 | Launch a subagent under a different model                 | 0093, 0097, 0099 |
