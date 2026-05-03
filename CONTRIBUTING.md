# Contributing

## Local checks

CI runs the unit suite and `mdbook build` on every push and PR. Run the same checks locally
before opening a PR:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## End-to-end tests

Tests gated behind `#[cfg(feature = "e2e")]` exercise real podman containers. They are not
run in CI. Locally:

```sh
cargo test --features e2e
```

Requires `podman` and `buildah` on `PATH` and `OPENAI_API_KEY` exported in the environment.

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
