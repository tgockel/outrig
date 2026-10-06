# `cargo fmt` never sees the CLI's module tree

## Symptom

`crates/outrig-cli/src/lib.rs` declares `cli`, `error`, `llm`, `repl`, `session` and the rest
through `internal_modules!`. That macro picks `pub` or `pub(crate)` depending on
`internal-test-api`. rustfmt does not expand macros, so it never reaches a module declared inside
one. `cargo fmt --all -- --check`, which is the CI gate and the PR-template check, therefore passes
over every file under those modules.

The drift is already there. Running `rustfmt --check --edition 2024` on
`crates/outrig-cli/src/cli/run.rs` reports seven hunks, and `build.rs`, `clean.rs`, `engine.rs`,
`logs.rs`, `ls.rs`, `mcp_self.rs`, `session_setup.rs` and `watcher.rs` each report some. All of it
predates the change that found it (#327), which formatted its own hunks by hand.

## Where it goes

Make the module tree visible to rustfmt. One way is to give rustfmt a path it can follow, for
example by listing the modules in a plain `mod` file the macro includes. The other is to point
the CI step at the files directly, as in
`rustfmt --check --edition 2024 $(git ls-files 'crates/*/src/**.rs')`. Then run rustfmt once over
the tree as a format-only commit, so the drift does not ride along with a behavior change.
