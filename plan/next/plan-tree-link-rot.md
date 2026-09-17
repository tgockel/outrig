# Nothing checks that a `plan/` path reference resolves

Fifteen paths written as `plan/...md` across `plan/` and `crates/` point at files that do not
exist. Every one predates the phase restructure, which found them rather than caused them: the
migration's own check compared the dead set before and after and added nothing to it.

They fall into two groups. Most name a `plan/next/` entry that was later promoted into the
numbered queue, so the reference kept the buffer path while the file moved and was renamed:

- `plan/next/dedup-init-tracing.md`, `plan/next/fix-e2e-test-compile-rot.md`,
  `plan/next/from-image-config-sidecar-translation.md`,
  `plan/next/global-workspace-block-dropped.md`, `plan/next/mount-errors-lack-provenance.md`,
  `plan/next/primary-view-relative-entrypoint.md`, `plan/next/public-api-snapshot-gate.md`,
  `plan/next/subagent-tree-shutdown-grace.md`, and
  `plan/next/e2e-imageconfig-sidecars-bitrot.md`.

The rest name entries that were never written at all: `plan/next/0090-config-surface-recheck.md`,
`plan/next/in-process-llm.md`, `plan/next/nested-podman-in-agent-containers.md`,
`plan/next/outrig-image-env-accessor.md`, `plan/next/primary-view-payload-home.md`, and
`plan/next/sidecar-primary-bootstrap-overlap.md`.

`plan/next/e2e-imageconfig-sidecars-bitrot.md` is the one that reaches outside the plan tree:
`crates/outrig-cli/tests/primary_view_e2e.rs` cites it too.

## Wanted

A check that every `plan/...md` path in the repository resolves, run in CI. `lychee` already
runs `--offline` over `doc/` and the root markdown files and does not look at `plan/`; the
cheapest version is to add the plan tree to that invocation, though lychee only sees markdown
link syntax and most of these references are backticked paths in prose. A dozen lines of
Python over `git ls-files` covers both shapes, and is what the migration used.

Whether the dead references should be repointed or deleted is per-reference. A promoted entry
should point at the task that absorbed it; an entry that was never written is a claim about
work that does not exist, and the sentence around it probably wants rewriting rather than
relinking.

## Why deferred

It is pre-existing and it breaks nothing at runtime. CocoClaw carries the same gap and filed
it the same way, in its own `plan/next/plan-tree-link-rot.md`, after three folder migrations
each left rotted links behind.
