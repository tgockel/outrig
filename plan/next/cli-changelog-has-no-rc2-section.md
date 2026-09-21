# The CLI changelog skipped rc.2, and its rc.1 link may name no tag

## Problem

`chore: prepare 0.2.0-rc.2` (`244485b4`) bumped `[workspace.package]`, the lockfile, and
`crates/outrig-cli/Cargo.toml`'s `outrig` requirement, and cut a dated `[0.2.0-rc.2]` section
in `crates/outrig/CHANGELOG.md`. It did not cut one in `crates/outrig-cli/CHANGELOG.md`.

The symptom is visible in the file: `[Unreleased]` carries two runs of the same subsection
headings, because the rc.2-era entries were never rolled up. The first run is post-rc.2 work,
the second is what a `[0.2.0-rc.2]` section would have held. Anything reading "what is
unreleased in the CLI" over-reports by a release.

Separately, `crates/outrig-cli/CHANGELOG.md`'s `## [0.2.0-rc.1]` heading links to the tag
`outrig-cli-v0.2.0-rc.1`. That tag does not exist in this clone. Only four do:

    outrig-v0.1.0  outrig-cli-v0.1.0  outrig-v0.2.0-rc.1  outrig-v0.2.0-rc.2

`RELEASING.md` step 8 tags each crate independently, so the CLI tags for both release
candidates are either unpushed, unfetched, or were never created. 0002-49 did not touch the
link, because confirming it needs the remote and a wrong "fix" would replace a dead link with
a false one.

## Sketch

- Check the remote and crates.io for what was actually published and tagged as `outrig-cli`
  at rc.1 and rc.2. That answers both halves.
- If the tags exist, the link is fine and only the missing `[0.2.0-rc.2]` heading needs
  cutting, splitting `[Unreleased]` at the boundary between the two runs of headings.
- If they do not, the rc.1 heading is making a claim with nothing behind it, and the release
  record for the CLI at rc.1 and rc.2 needs reconstructing rather than repairing.

Either way the heading work belongs to a release task: `0002-52` cuts rc.3 and `0002-54` folds
the candidate sections into `[0.2.0]`, and both already own dated headings and tag links.

## Dependencies

- Wants the remote. Not actionable from a sandbox with no push access.
- Belongs with `plan/todo/0002-52-cut-0.2.0-rc.3.md` or `plan/todo/0002-54-release-0.2.0.md`.
