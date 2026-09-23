# Three declared dependencies have no consumer

## Context

Three `[dependencies]` entries compile a crate that nothing in the declaring crate uses:

| Crate        | Dependency     | Declared at                          |
|--------------|----------------|--------------------------------------|
| `outrig-cli` | `regex`        | `crates/outrig-cli/Cargo.toml`       |
| `outrig`     | `async-stream` | `crates/outrig/Cargo.toml`           |
| `outrig`     | `walkdir`      | `crates/outrig/Cargo.toml`           |

All three date from `361c6db`, which split the one crate into `outrig` and `outrig-cli` and
copied declarations into both halves. `regex` is used by `outrig` and not by `outrig-cli`.
`async-stream` and `walkdir` were used only by `outrig-cli`: by the in-process backend's
streaming loop and by its smoke test. When the 0.3 line removed that backend it took both off
`outrig-cli`'s manifest, and `outrig`'s copies are now the only direct declarations.

The cost is manifest hygiene rather than build time. Both stay in the graph either way --
`ignore` pulls `walkdir` and `rig-core` pulls `async-stream` -- so a declaration that nothing
uses misleads a reader about what the crate depends on and compiles nothing extra.

Found while removing that backend. Left alone there because neither removal is the backend's,
and the first predates it entirely.

## Sketch

- Drop the three declarations. Drop the workspace pins for `async-stream` and `walkdir`, which
  then have no user. `regex`'s pin stays, since `outrig` uses it.
- A `cargo machete` or `cargo udeps` CI step would stop this recurring. It belongs with
  `plan/next/ci-configuration-coverage.md`'s cargo-hack job, which asks the same question for
  features.

## Acceptance

- `grep -rn 'async_stream\|walkdir' crates/` and `grep -rn 'regex' crates/outrig-cli/src` return
  nothing, and none of the three is declared where it is not used.
- `cargo tree -p outrig -e normal --depth 1` no longer lists `async-stream` or `walkdir`, and
  `cargo tree -p outrig-cli -e normal --depth 1` no longer lists `regex`.
