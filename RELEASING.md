# Releasing

OutRig publishes two crates from this workspace: the `outrig` library and the `outrig-cli`
binary. Releases are cut manually. This document is the checklist.

`outrig-cli` depends on `outrig` (`outrig = { path = "../outrig", version = "0.1.0" }`), so
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

3. **Update the CHANGELOG.** Put the released items under a dated `[X.Y.Z]` section and set
   the date to today. Confirm the link reference at the bottom points at the tag you will
   create.

4. **Dry-run the library** -- catches missing files, bad README paths, and a dirty tree:

   ```sh
   cargo publish --dry-run -p outrig
   ```

   `outrig-cli` cannot be dry-run yet: its verify build resolves the `outrig` dependency from
   the crates.io index, which does not have the new version until step 5. It is validated by
   its own publish below.

5. **Publish, library first:**

   ```sh
   cargo publish -p outrig
   # wait for the new version to appear in the crates.io index, then:
   cargo publish -p outrig-cli
   ```

   `cargo publish` verify-builds each crate before uploading, so `outrig-cli` is checked as
   part of its own publish. It builds against the just-published `outrig`, so it can fail if
   the index has not updated yet -- wait a minute and retry if so.

6. **Flip the install docs.** Remove the `TODO: Incomplete` note above the
   `cargo install outrig-cli` block in [`doc/quickstart.md`](doc/quickstart.md) now that the
   crate is live, and commit.

7. **Tag and push:**

   ```sh
   git tag -a vX.Y.Z -m "outrig X.Y.Z"
   git push origin vX.Y.Z
   ```

8. **GitHub release.** Create a release from the tag and paste the CHANGELOG section for this
   version. The `docs.yml` workflow already deploys the docs site on push to `trunk`.

9. **Smoke-test the published artifact:**

   ```sh
   cargo install outrig-cli
   outrig --version    # prints outrig X.Y.Z
   ```

## Notes

- The default build pulls no heavy ML dependencies. `local-llm`, `cuda`, and `metal` are
  opt-in features and are not exercised by the `cargo publish` verify build.
- See [CONTRIBUTING.md](CONTRIBUTING.md) for the local checks run before every PR.
