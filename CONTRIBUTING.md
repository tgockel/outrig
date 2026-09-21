# Contributing

## Local checks

Install the musl target matching your architecture once before building:

```sh
rustup target add x86_64-unknown-linux-musl   # or aarch64-unknown-linux-musl
```

`crates/outrig/build.rs` compiles the `outrig-enter` launcher for it. Without the target the
build still succeeds -- it emits a warning and embeds an empty artifact -- but every
`view = "primary"` sidecar fails at session start, including the three this repo's own
`.agents/outrig/config.toml` declares. Development happens on Linux: macOS and native Windows
are not supported, and WSL2 is an ordinary Linux build.

CI runs the unit suite and `mdbook build` on every push and PR. Run the same checks locally
before opening a PR:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Public-API snapshots

`crates/outrig/public-api.txt` and `crates/outrig-cli/public-api.txt` are generated records of
each crate's published surface, and CI fails when either has drifted from the code:

```sh
python3 scripts/check-public-api.py --install-missing
```

It needs **Python 3.11 or newer** (it reads the pins with `tomllib`), which is newer than
`scripts/audit-doc-style.py` asks for -- on Ubuntu 22.04 reach for a `python3.11` rather than
the default `python3`. It is kept out of the block above because the first run installs a
pinned nightly and a pinned `cargo-public-api` into a cache directory, which takes a few
minutes; without `--install-missing` the script reports what is absent instead of downloading
anything. Both
pins live in `[workspace.metadata.public-api]` in the root `Cargo.toml`, and the exit code
separates the two ways this fails -- 1 when the surface differs, 2 when the tooling does.

The check does not forbid changing the API. After an intentional surface change, regenerate
with `python3 scripts/check-public-api.py --write` and let the regenerated snapshot travel in
the same commit: that diff is the review material.

## End-to-end tests

Tests gated behind `#[cfg(feature = "e2e")]` exercise real podman containers. CI compiles
them but does not run them -- the runners have no podman, so the `e2e` matrix row stops at
`cargo test --no-run`. Compilation failures are caught; behavior regressions are not. Run the
full suite yourself with:

```sh
cargo test --workspace --features outrig/e2e,outrig-cli/e2e
```

Prerequisites: rootless `podman` + `buildah` on `PATH`. The two run-time tests have
different requirements:

- `quickstart_mocked` (always runs under `--features e2e`) needs only podman/buildah. It
  drives `outrig init -> build -> run` end-to-end against a hand-rolled mock OpenAI
  server, so no API key or network egress is required for the LLM leg. Expect ~30-60 s
  with a warm image cache; the first cold run can take several minutes for `buildah` to
  pull and assemble the default Debian-based image.
- `quickstart_real_api` is opt-in. Run it with both `OUTRIG_E2E_REAL_API=1` and a real
  `OPENAI_API_KEY` exported to exercise the live OpenAI endpoint. The cost is negligible
  (~$0.01 of `gpt-4o-mini` per run); the test retries the run leg up to twice to absorb
  LLM non-determinism.
- The remaining e2e tests (`run_smoke`, `container_add_buildable`, `image_build_smoke`,
  `container_lifecycle`, etc.) only need podman/buildah.

## Documentation

Docs live in `doc/` and render as an mdbook:

```sh
cargo install mdbook mdbook-mermaid --locked
mdbook serve   # live preview on http://localhost:3000
mdbook build   # static HTML in target/book/
```

CI also runs [lychee](https://lychee.cli.rs) against the markdown sources to catch broken
intra-doc links (`lychee --offline 'doc/**/*.md' README.md CONTRIBUTING.md`).

## Workflow

The implementation queue lives in [`plan/todo/`](plan/todo/README.md); design docs live in
[`doc/`](doc/README.md). The harness conventions for advancing the queue are described in
[`.claude/CLAUDE.md`](.claude/CLAUDE.md).

## Releasing

Cutting a release (tagging and publishing to crates.io) is documented in
[`RELEASING.md`](RELEASING.md).
