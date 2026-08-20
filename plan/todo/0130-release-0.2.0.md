# 0130 -- Release 0.2.0

## Context

The queue up to here fixes the blockers, corrects the docs, cuts `0.2.0-rc.3` (0128), and gathers
live runtime evidence (0129). Then it stopped. Nothing joined those two into a final release, and
the two tasks nearest the end actively contradict each other if read as the end: 0126 was written
to produce final `[0.2.0]` documentation, while 0128 explicitly cuts an RC, says it is not final,
and requires integration time afterward. Something has to own the decision that the integration
time was enough, and the mechanics that follow it.

That is this task. It is deliberately last, deliberately gated on a judgment rather than on a
test, and deliberately separate from 0128 so that a second RC -- if rc.3 exposes another
public-surface correction -- costs one more pass through 0128 rather than a rewrite of the final.

The audit's own sequencing says the same thing: cut rc.3, exercise it, "release 0.2.0 only after
rc.3 gets integration time with no further public-surface corrections."

## Goal

0.2.0 published from a tree whose public surface has not moved since rc.3, with the release
mechanics done in the order `RELEASING.md` requires.

## The exit criteria

Written down before the soak starts, not judged afterward. The release proceeds only if all hold:

- **No public-surface change since rc.3.** `crates/outrig/public-api.txt` and
  `crates/outrig-cli/public-api.txt` are byte-identical to the rc.3 tag under 0125's pinned
  toolchain. A single moved line means another RC, not a judgment call about severity.
- **0129's evidence exists** for both architectures and is green, including 0114's live tier.
- **0128's recorded soak parameters are satisfied** -- the window, the qualifying use, the
  evidence source, and the blocker policy, all decided in 0128 before rc.3 shipped. This task
  checks that record; it does not write it. An RC nobody installed has not been
  integration-tested, however many days elapsed.
- **No open defect of the class 0128's record calls blocking.**

If any fails, the outcome is a new RC through 0128, and this task waits.

## Deliverables

- **The rc.3 -> 0.2.0 bump**: `[workspace.package]` in the root `Cargo.toml` and the `outrig`
  requirement in `crates/outrig-cli/Cargo.toml`, which must drop the pre-release from the pin.
- **Final documentation**, the half 0126 deliberately did not write because it can only be
  written once:
  - dated `[0.2.0]` headings in both changelogs, with tag links pointing at `outrig-v0.2.0` and
    `outrig-cli-v0.2.0`, folding rc.1 + rc.2 + `Unreleased` into one section per crate;
  - the migration checklist 0126 drafted, promoted into that section;
  - the quickstart's sample `outrig --version` output;
  - `SECURITY.md`'s supported-version prose and table, which `RELEASING.md` step 6 says to skip
    for a pre-release and therefore has been skipped through three RCs;
  - no file describing 0.2.0 as a candidate.
- **Final package validation**, re-run rather than assumed to still hold: 0125's snapshot gate,
  0128's version guard, 0128's install-shaped check, and the combined
  `cargo publish --dry-run -p outrig -p outrig-cli`.
- **Publication in dependency order** -- `outrig`, then `outrig-cli` once the index has caught up
  -- followed by the post-publish `cargo install outrig-cli` smoke test, the two tags, and the
  GitHub release, per `RELEASING.md` steps 5-9.

## Acceptance

- The exit criteria above are each recorded as met, with the evidence, in this task's
  `## Decisions`. "We felt it was ready" is not a record.
- **The release record states gate item 17 as partially met.** 0125 accepted the downstream
  runtime-core surface test as a deliberate exception, so a note claiming the 18-item gate was
  completed in full would be false. Carry the waiver forward with its reason.
- `cargo install outrig-cli --version 0.2.0 --locked` into a clean `CARGO_HOME` and a clean
  `--root` yields a binary reporting `0.2.0`, and the resolved `outrig` is `0.2.0` **from
  crates.io** -- assert the source, not just the version, or a stale local path satisfies it.
- Both tags exist and match the changelog links.
- **No *active* artifact describes a release candidate**: the two manifests and the lockfile, the
  current changelog headings and their tag links, the quickstart's version output,
  `SECURITY.md`'s supported-version table, and the release announcement. Historical changelog
  sections, `RELEASING.md`'s pre-release guidance, `plan/done/` entries, and this task all
  discuss RCs legitimately and are out of scope -- "nothing in the tree says `rc`" was an earlier
  draft's criterion and could never have passed.
- The published `outrig` crate contains `src/container/enter/launcher.rs`, which `build.rs`
  compiles at build time -- check the actual `.crate` archive, since the project's constraint is
  that `cargo install` builds every feature from source with no prebuilt binaries.

## Design forks

1. **How long the soak is -- Open, decide before rc.3 ships, not after.** A week of real use
   beats a month of nobody installing it. The criterion that matters is whether anyone integrated
   against rc.3, so the window should be stated in terms of that and not only in days.

2. **What counts as a public-surface change worth another RC -- Recommended: any.** A byte-diff
   in the snapshot is a bright line and cheap to check. Softening it to "any *breaking* change"
   reintroduces the judgment call this task exists to avoid, and 0125 exists precisely because
   the snapshot has silently rotted before.

## Dependencies

- **Hard: 0128** -- there is no final without the RC it cuts.
- **Hard: 0129** -- the runtime evidence is an exit criterion.
- **Hard: 0125** -- the snapshot comparison is only meaningful under a pinned toolchain.

## See also

- `RELEASING.md` -- steps 5 through 9 are the mechanics; step 6's pre-release skip is why the
  version-bearing docs are still unwritten.
- `plan/todo/0126-documentation-contracts-and-one-migration-guide.md` -- drafted the migration
  material this publishes.
- `plan/todo/0128-cut-0.2.0-rc.3.md` -- the RC, and the loop this task returns to on failure.
