# Gate the public-API snapshots instead of trusting the honor system

## Context

`crates/outrig/public-api.txt` and `crates/outrig-cli/public-api.txt` are the artifact the whole
0093-0095 hardening arc is measured against, and nothing enforces them. Their header says
"Regenerate after any intentional surface change and review the diff"; 0093's Decision 14
deliberately added no CI job, on the grounds that rustdoc's JSON format shifts between nightlies
and would break CI on tool churn rather than on real changes.

The cost showed up in 0095. Regenerating the snapshot dropped 87 lines that were never a surface
change: 0094 had left the whole `#[non_exhaustive]` block in the file twice -- once as a leading
block above the `pub mod outrig` header, once in its sorted position. It survived a full task and
was noticed only because a later task happened to regenerate the file. A snapshot nobody
regenerates is a snapshot that silently rots, which is the failure mode it exists to prevent.

## Goal

A surface change that forgets to regenerate the snapshot fails something, without CI breaking every
time nightly's rustdoc JSON moves.

## Sketch

- A test gated behind a feature (`surface-snapshot`, off by default) that runs the pinned
  `cargo public-api` invocation and diffs against the checked-in file. Local and opt-in, so nightly
  churn never blocks an unrelated PR.
- Or a `scripts/` entry alongside `audit-doc-style.py` that does the same, wired into the release
  checklist in `RELEASING.md` rather than into per-PR CI -- the snapshot only has to be true at the
  moment a version is cut.
- Either way the pinned tool version lives in one place. Today it is prose in two file headers, so a
  regeneration on a different version is indistinguishable from a real diff.

## Notes

Worth doing before `0.2.0` is cut, since that is the release the snapshots are supposed to be the
record of.

Related: `plan/done/0093-shrink-reachable-surface.md` (Decision 14, the no-CI-job call this
revisits), `plan/done/0094-non-exhaustive-sweep.md` (where the duplicate block came from).
