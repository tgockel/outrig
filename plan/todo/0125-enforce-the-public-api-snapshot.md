# 0125 -- Gate the public-API snapshots instead of trusting the honor system

## Context

`crates/outrig/public-api.txt` and `crates/outrig-cli/public-api.txt` are the artifact the whole
0093-0095 hardening arc is measured against, and nothing enforces them. Their header says
"Regenerate after any intentional surface change and review the diff"; 0093's Decision 14
deliberately added no CI job, on the grounds that rustdoc's JSON format shifts between nightlies
and would break CI on tool churn rather than on real changes.

The cost showed up in 0095. Regenerating the snapshot dropped 87 lines that were never a surface
change: 0094 had left the whole `#[non_exhaustive]` block in the file twice -- once as a leading
block above the `pub mod outrig` header, once in its sorted position. It survived a full task and
was noticed only because a later task happened to regenerate the file.

The 0.2.0 audit measured the same rot again, from the other direction: regenerating found six
`std::io` versus `core::io` rendering differences. Those are not real breaks -- and that is the
finding. The snapshot cannot be assumed current, so a genuine break hiding among rendering noise
would not stand out. A snapshot nobody regenerates is a snapshot that silently rots, which is the
failure mode it exists to prevent, and 0.2.0 is the release these files are supposed to be the
record of.

## Goal

A surface change that forgets to regenerate the snapshot fails something, without CI breaking
every time nightly's rustdoc JSON moves.

## Deliverables

- **The pinned toolchain lives in exactly one place.** Today it is prose in two file headers, so
  a regeneration on a different `cargo public-api` or a different nightly is indistinguishable
  from a real diff -- which is how the six `std::io` lines got there. Pin both the tool version
  and the rustdoc/nightly, once, somewhere the check reads.
- **Something mechanical invokes it.** A script mentioned only in `RELEASING.md` fails nothing
  unless a human remembers to run it -- that is a better command, not enforcement, and the
  failure mode being fixed is precisely that nobody remembered. Once the exact nightly and the
  exact `cargo-public-api` version are pinned, the nightly-churn objection no longer applies,
  because the job no longer floats. So: a pinned-toolchain PR CI job, or an enforced release
  workflow that cannot be skipped. Fork 1 picks which.
- **The invocation is machine-readable configuration, not prose.** Exact nightly date,
  `cargo-public-api` version and how it is installed, target triple, and feature set, all in one
  file the job and a developer both read. Generate into a temporary file and diff against the
  checked-in one, so a failed run does not leave the tree dirty.
- **`RELEASING.md` gains the step** regardless, before the publish dry-run -- the snapshot has to
  be true at the moment a version is cut even if CI already checked it.
- **Regenerate both snapshots as part of this task**, on the newly pinned toolchain, so the
  checked-in files are the pinned tool's output rather than an older one's. Review the diff: the
  surface moves from 0118-0124 land in it, and the six rendering differences should disappear as
  noise rather than be committed as changes.

## Acceptance

- Deleting a `pub fn` from the library makes the check fail, naming the item. **Adding** one does
  too -- an additive surface change is still a surface change, and a check that only catches
  removals lets the snapshot drift in the direction it actually drifts.
- The check runs from a clean environment through its real entry point, with no pinned tooling
  preinstalled, so "works on a machine that already had it" is not the thing being verified.
- Running the check twice on an unmodified tree produces no diff -- the determinism claim the
  pinning exists to make.
- `RELEASING.md`'s checklist includes the step, positioned so a failure is caught before
  anything is published.
- The two `public-api.txt` headers no longer carry the pinned version as prose, or carry it as a
  pointer to the single source.

## Design forks

1. **Pinned PR job versus enforced release job -- Open, but one of them must exist.** A PR job
   catches the omission at the moment it is made and costs a nightly toolchain install on every
   run; because the nightly is pinned to a date, it breaks only when someone bumps that date
   deliberately, which is the churn 0093's Decision 14 was worried about and is no longer
   accidental. A release-workflow job is cheaper and lets a wrong snapshot live on `trunk`
   between releases. What is not on the table is a script nothing invokes.

2. **Whether a local opt-in test is also worth it -- Recommended: yes, cheap.** A test behind an
   off-by-default `surface-snapshot` feature lets a developer check their own work mid-task
   without pushing. It shares the pinned configuration, so it is a second entry point rather
   than a second source of truth.

## The downstream surface test

Gate item 17 asks for two things: snapshot enforcement *and* downstream-style compile and runtime
surface tests. This task owns the first. The second is
`plan/next/container-surface-test.md` -- a test that drives `container`/`image`/`mcp_proxy`/
`network` the way an external consumer does, so a breaking change fails a test rather than a
downstream build.

**Decided: deferred, as an accepted release exception.** 0.2.0 ships snapshot enforcement without
the runtime-core surface test. The rationale is that the surface is not untested, only untested
*as a whole*: 0118, 0119, and 0122 each add external, out-of-crate tests against the parts they
change, and 0124's sealing test pins the trait boundary. What is genuinely uncovered is the
composed path -- acquire an image, start a container from a spec, exec a server over it,
aggregate through `ProxyServer`, tear down -- driven the way a downstream crate drives it. A
snapshot proves that shape did not move; it does not prove the shape is still usable, and 0095's
churn is already in the tree waiting for something to exercise it.

That is a real gap and it is being accepted, not closed. Two consequences follow, and both are
obligations on this task rather than notes:

- **This task records the exception in its `## Decisions`**, with the reasoning above, so the
  waiver is recoverable later.
- **0129's release record says gate item 17 was *partially* met**, not met. If the final notes
  claim the release gate was completed in full, this decision has been quietly reversed.

## Dependencies

- **Soft: after 0118-0124.** Regenerating before the pre-freeze surface changes land means doing
  it twice. If this task is taken earlier, the regeneration step moves to whichever surface task
  lands last.

## See also

- `crates/outrig/public-api.txt`, `crates/outrig-cli/public-api.txt` -- the artifacts and their
  headers.
- `plan/done/0093-shrink-reachable-surface.md` (Decision 14, the no-CI-job call this revisits),
  `plan/done/0094-non-exhaustive-sweep.md` (where the duplicate block came from),
  `plan/done/0095-options-structs-and-sealing.md` (where it was found).
- `scripts/audit-doc-style.py` -- the shape a `scripts/` entry would follow.
