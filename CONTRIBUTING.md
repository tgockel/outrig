# Contributing

## Local checks

CI runs the unit suite and `mdbook build` on every push and PR. Run the same checks locally
before opening a PR:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

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
