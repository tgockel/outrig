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

1. **Green main.** `trunk` passes CI and the working tree is clean (`git status` is empty).

2. **Bump the version** if needed. The version is set once in the root `Cargo.toml` under
   `[workspace.package]` and inherited by both crates. Update the `version = "X.Y.Z"` in
   `crates/outrig-cli/Cargo.toml`'s `outrig = { ... }` dependency to match.

3. **Update the CHANGELOGs.** Each crate keeps its own changelog:
   [`crates/outrig/CHANGELOG.md`](crates/outrig/CHANGELOG.md) and
   [`crates/outrig-cli/CHANGELOG.md`](crates/outrig-cli/CHANGELOG.md). Move each crate's
   released items under a dated `[X.Y.Z]` section, set the date to today, and confirm the
   section's tag link points at that crate's tag (`outrig-vX.Y.Z` /
   `outrig-cli-vX.Y.Z`).

4. **Dry-run both crates in one invocation** -- catches missing files, bad README paths, a
   dirty tree, and a stale `outrig` requirement in `outrig-cli`:

   ```sh
   cargo publish --dry-run -p outrig -p outrig-cli
   ```

   Cargo packages both, unpacks `outrig` into a temporary registry under `target/package/`,
   and verify-builds `outrig-cli` against that packaged copy -- so the dependency
   requirement is genuinely exercised without `outrig` being on the index.

   Dry-running `outrig-cli` *on its own* fails until the matching `outrig` is published:
   cargo resolves the requirement against crates.io and reports `failed to select a version
   for the requirement`. That is expected, not a fault in the crate.

5. **Publish.** One invocation does both in dependency order, waiting for the index between
   them:

   ```sh
   cargo publish -p outrig -p outrig-cli
   ```

   Cargo verify-builds each crate before uploading. To do them separately instead, publish
   `outrig` first and wait for the new version to reach the index before running
   `cargo publish -p outrig-cli` -- that build resolves against the real index and fails if
   the index has not caught up.

6. **Refresh the version-bearing docs.** Update the sample `outrig --version` output in
   [`doc/quickstart.md`](doc/quickstart.md) and the supported-version prose and table in
   [`SECURITY.md`](SECURITY.md), then commit. Skip this step for a pre-release -- a bare
   `cargo install outrig-cli` still lands on the latest stable, so both files should go on
   describing that.

7. **Tag and push.** Tag each crate independently, matching the links in the changelog
   headers:

   ```sh
   git tag -a outrig-vX.Y.Z     -m "outrig X.Y.Z"
   git tag -a outrig-cli-vX.Y.Z -m "outrig-cli X.Y.Z"
   git push origin outrig-vX.Y.Z outrig-cli-vX.Y.Z
   ```

8. **GitHub release.** Create a release from the `outrig-cli-vX.Y.Z` tag (the user-facing
   artifact) and paste its `crates/outrig-cli/CHANGELOG.md` section for this version,
   noting the library's `crates/outrig/CHANGELOG.md` section as well. The `docs.yml`
   workflow already deploys the docs site on push to `trunk`.

9. **Smoke-test the published artifact:**

   ```sh
   cargo install outrig-cli
   outrig --version    # prints outrig X.Y.Z
   ```

## Pre-releases

Cut a release candidate (`X.Y.Z-rc.N`) when a cycle breaks the public surface, so downstream
consumers can integration-test before the version becomes the recommended one. The checklist
above applies, with these differences:

- **The dependency pin must carry the exact pre-release.** Cargo excludes pre-releases from
  ordinary version ranges -- a requirement of `0.2.0` does not match a published
  `0.2.0-rc.1`, which sorts below it. So `crates/outrig-cli/Cargo.toml` needs
  `outrig = { path = "../outrig", version = "X.Y.Z-rc.N" }`, exactly.
- The combined dry-run in step 4 does exercise that pin -- it verify-builds `outrig-cli`
  against a packaged `outrig` in a temporary registry, so a stale requirement fails there
  rather than at publish time.
- Tags and changelog header links carry the full version: `outrig-vX.Y.Z-rc.N`.
- Mark the GitHub release with `gh release create --prerelease`, which keeps it out of the
  repository's "Latest release" slot.
- Step 6 is skipped; see the note there.
- The smoke test needs an explicit version, because a bare install will not resolve to a
  pre-release: `cargo install outrig-cli --version X.Y.Z-rc.N`.

Consumers opt in with `outrig = "X.Y.Z-rc.N"`. At final release, drop the `-rc.N` from both
manifests, rename the changelog headers from `[X.Y.Z-rc.N]` to `[X.Y.Z]` (folding in whatever
the rc cycle turned up), and do step 6.

## Notes

- The default build pulls no heavy ML dependencies. `local-llm`, `cuda`, and `metal` are
  opt-in features and are not exercised by the `cargo publish` verify build. All three are
  **deprecated** and scheduled for removal; `build.rs` warns when `local-llm` is enabled.
- See [CONTRIBUTING.md](CONTRIBUTING.md) for the local checks run before every PR.
