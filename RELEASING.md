# Releasing

OutRig publishes two crates from this workspace: the `outrig` library and the `outrig-cli`
binary. Releases are cut manually. This document is the checklist.

`outrig-cli` depends on `outrig` (`outrig = { path = "../outrig", version = "X.Y.Z" }`), so
the library is published first and the CLI second.

## Before you start

- You need crates.io publish rights for both `outrig` and `outrig-cli`, with a token
  configured (`cargo login`).
- Confirm both crate names are owned or available on crates.io.

## Checklist

Work through these in order. They are headings rather than a numbered list so that inserting a
step does not renumber the ones below it, and so the cross-references elsewhere in this file
are links CI can check rather than prose that rots silently.

### Green main

`trunk` passes CI and the working tree is clean (`git status` is empty). The packaging steps
below need it clean for their own reasons: `cargo package` refuses to archive uncommitted
changes, since what it would publish is not what anyone can check out.

### Bump the version

The version is set once in the root `Cargo.toml` under `[workspace.package]` and inherited by
both crates. Update the `version = "X.Y.Z"` in `crates/outrig-cli/Cargo.toml`'s
`outrig = { ... }` dependency to match, then regenerate the lockfile.

**Never reuse a version.** A published version is immutable, and yanking does not free the
number. `cargo publish` refuses outright, so the cost of getting this wrong is a wasted verify
build rather than a bad release -- but a tree left claiming an already-published version cannot
be released at all until someone notices, which is worth one glance at the crate's versions
page before starting.

### Update the CHANGELOGs

Each crate keeps its own changelog:
[`crates/outrig/CHANGELOG.md`](crates/outrig/CHANGELOG.md) and
[`crates/outrig-cli/CHANGELOG.md`](crates/outrig-cli/CHANGELOG.md). Move each crate's
released items under a dated `[X.Y.Z]` section, set the date to today, and confirm the
section's tag link points at that crate's tag (`outrig-vX.Y.Z` / `outrig-cli-vX.Y.Z`).

### Verify the public-API snapshots

Regenerate both `public-api.txt` files on the pinned toolchain and diff them against what is
committed:

```sh
python3 scripts/check-public-api.py
```

CI runs this on every PR, so it is normally already true; running it here is what makes it
true *of the commit being cut*. A difference means either that the surface moved without
the snapshot or that a pin moved, and the exit code says which -- 1 for a surface
difference, 2 for a tooling fault. Regenerate a real one with `--write` and review the diff
before continuing.

### Dry-run both crates in one invocation

Catches missing files, bad README paths, a dirty tree, and a stale `outrig` requirement in
`outrig-cli`:

```sh
cargo publish --dry-run -p outrig -p outrig-cli
```

Cargo packages both, unpacks `outrig` into a temporary registry under `target/package/`,
and verify-builds `outrig-cli` against that packaged copy -- so the dependency
requirement is genuinely exercised without `outrig` being on the index.

Dry-running `outrig-cli` *on its own* fails until the matching `outrig` is published:
cargo resolves the requirement against crates.io and reports `failed to select a version
for the requirement`. That is expected, not a fault in the crate.

### Publish

One invocation does both in dependency order, waiting for the index between them:

```sh
cargo publish -p outrig -p outrig-cli
```

Cargo verify-builds each crate before uploading. To do them separately instead, publish
`outrig` first and wait for the new version to reach the index before running
`cargo publish -p outrig-cli` -- that build resolves against the real index and fails if
the index has not caught up.

### Refresh the version-bearing docs

Update the sample `outrig --version` output in
[`doc/quickstart.md`](doc/quickstart.md) and all three version sites in
[`SECURITY.md`](SECURITY.md) -- the supported-version prose, its table, and the
`0.X.x` in **Reporting a vulnerability** that says where a fix ships -- then commit.
The third one is easy to miss; a `grep` for the previous minor over both files is
what catches it. Skip this step for a pre-release -- a bare
`cargo install outrig-cli` still lands on the latest stable, so both files should go on
describing that.

### Tag and push

Tag each crate independently, matching the links in the changelog headers:

```sh
git tag -a outrig-vX.Y.Z     -m "outrig X.Y.Z"
git tag -a outrig-cli-vX.Y.Z -m "outrig-cli X.Y.Z"
git push origin outrig-vX.Y.Z outrig-cli-vX.Y.Z
```

### GitHub release

Create a release from the `outrig-cli-vX.Y.Z` tag (the user-facing artifact) and paste its
`crates/outrig-cli/CHANGELOG.md` section for this version, noting the library's
`crates/outrig/CHANGELOG.md` section as well. The `docs.yml` workflow already deploys the
docs site on push to `trunk`.

### Smoke-test the published artifact

```sh
cargo install outrig-cli
outrig --version    # prints outrig X.Y.Z
```

## Pre-releases

Cut a release candidate (`X.Y.Z-rc.N`) when a cycle breaks the public surface, so downstream
consumers can integration-test before the version becomes the recommended one. The checklist
above applies, with these differences:

- **A candidate is library-only.** Publish `outrig`; leave `outrig-cli` unpublished, keeping
  its changelog on `[Unreleased]` while it inherits the workspace version, and tag only
  `outrig-vX.Y.Z-rc.N`. A candidate exists so a library consumer can compile against the new
  surface, and anyone wanting the CLI at a candidate can install it from git. This is what
  0.2.0-rc.1 and rc.2 both did. Publish both crates at the final release.
- **The dependency pin must carry the exact pre-release.** Cargo excludes pre-releases from
  ordinary version ranges -- a requirement of `0.2.0` does not match a published
  `0.2.0-rc.1`, which sorts below it. So `crates/outrig-cli/Cargo.toml` needs
  `outrig = { path = "../outrig", version = "X.Y.Z-rc.N" }`, exactly.
- [Dry-run both crates in one invocation](#dry-run-both-crates-in-one-invocation) exercises
  that pin -- it verify-builds `outrig-cli` against a packaged `outrig` in a temporary
  registry, so a requirement that resolves to nothing fails there rather than at publish
  time. It does not catch a merely *stale* one: `X.Y.Z-rc.(N-1)` still satisfies the range, so
  it packages cleanly and publishes a looser requirement than intended. Cargo resolves such a
  requirement to the newest matching version, so this costs precision rather than
  correctness -- but update the pin with the version, not after someone notices.
- The library's tag and changelog header link carry the full version: `outrig-vX.Y.Z-rc.N`.
- Mark the GitHub release with `gh release create --prerelease`, which keeps it out of the
  repository's "Latest release" slot.
- The [Refresh the version-bearing docs](#refresh-the-version-bearing-docs) step is skipped;
  see the note there.
- [Smoke-test the published artifact](#smoke-test-the-published-artifact) has nothing to
  install, since the CLI was not published. The combined dry-run above is what stands in for
  it: it builds the CLI candidate against the library being cut.

Consumers opt in with `outrig = "X.Y.Z-rc.N"`. At final release, drop the `-rc.N` from both
manifests, rename the library's changelog header from `[X.Y.Z-rc.N]` to `[X.Y.Z]` and fold the
CLI's `[Unreleased]` into its own `[X.Y.Z]` (taking in whatever the rc cycle turned up), and do
the [Refresh the version-bearing docs](#refresh-the-version-bearing-docs) step.

## Notes

- See [CONTRIBUTING.md](CONTRIBUTING.md) for the local checks run before every PR.
