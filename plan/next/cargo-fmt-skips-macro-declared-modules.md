# `cargo fmt --all` silently skips 40 files, including all of `repl/`

## Symptom

`cargo fmt --all -- --check` -- the CI gate (`.github/workflows/ci.yml:17`) and the command
`CONTRIBUTING.md:21` tells contributors to run -- exits 0 on a tree containing files rustfmt
would rewrite. Asked about one of them directly, rustfmt disagrees:

```sh
$ cargo fmt --all -- --check ; echo $?
0
$ rustfmt --edition 2024 --check crates/outrig-cli/src/repl.rs ; echo $?
Diff in /workspace/crates/outrig-cli/src/repl.rs:221:
-                    if trimmed.is_empty() {
+            if trimmed.is_empty() {
1
```

The gate is not weakly enforced; for these files it does not run at all.

## Scope

40 of 136 workspace `.rs` files are never visited. For `outrig-cli/src` it is 39 of 56 --
the majority of the crate, including `cli/run.rs`, all of `llm/`, all of `init/`, `error.rs`,
and the whole of `repl/`.

Measured with `cargo fmt --all -- --check -v`, which prints a `Formatting <path>` line per
file it opens:

```sh
cargo fmt --all -- --check -v 2>&1 | grep '^Formatting' | sed 's/Formatting //' | sort > seen
find crates -name '*.rs' -not -path '*/target/*' | sort > disk
comm -23 disk seen   # 40 paths
```

9 of the 40 are misformatted today (`cli/ls.rs`, `cli/run.rs`, `cli/session_setup.rs`,
`config_init.rs`, `image_setup/init.rs`, `llm.rs`, `llm/failover.rs`, `llm/retry.rs`, and
`outrig/src/container/enter/launcher.rs`), plus `repl.rs`, fixed on this branch where it was
found. The rest are merely unprotected: they are correct now and nothing would report it if
they stopped being.

Count them by file rather than by passing `mod.rs` to rustfmt -- given a module root it
recurses into the children, so `cli/mod.rs` reports diffs that belong to `cli/ls.rs` and is
itself clean.

## Cause

rustfmt discovers files by walking `mod` declarations from each crate root, and it parses
that tree *without expanding macros*. `lib.rs` declares most of `outrig-cli` through the
`internal_modules!` macro added for `internal-test-api` visibility
(`crates/outrig-cli/src/lib.rs:15-31`):

```rust
macro_rules! internal_modules {
    ($($name:ident),+ $(,)?) => { $(
        #[cfg(feature = "internal-test-api")] pub mod $name;
        #[cfg(not(feature = "internal-test-api"))] pub(crate) mod $name;
    )+ };
}
internal_modules! { cli, config_init, error, hf, image_setup, init, llm, repl, ... }
```

The `mod` items exist only after expansion, so rustfmt never learns those modules exist, and
with them their entire subtrees. The unvisited list is exactly the `internal_modules!` names
plus their descendants -- which is why `mcp_self/`, `subagent/`, `paths.rs` and the other
plainly-declared modules *are* covered.

Confirmed on a three-file scratch crate: one module declared by macro, one by a plain `mod`,
both misformatted identically. rustfmt reports only the plain one.

`outrig/src/container/enter/launcher.rs` is the same class of miss for a different reason --
it has no `mod` declaration anywhere, being pulled in by `include!` from the musl build.

## Why it went unnoticed

The macro predates this: it is the mechanism `lib.rs` describes for `internal-test-api`, and
nothing about it suggests a formatting consequence. The gate keeps passing, so the signal a
contributor gets is indistinguishable from a clean tree. `repl.rs`'s stale indentation
reached a branch this way -- an artifact of extracting the loop body into `run_loop`, which
`cargo fmt` would have caught on any plainly-declared file.

## Suggested fix

Decide the enforcement mechanism first; it determines whether the macro can stay.

- **Format by file list rather than by module tree.** Something like
  `find crates -name '*.rs' -not -path '*/target/*' -print0 | xargs -0 rustfmt --edition 2024
  --check` in CI and in `CONTRIBUTING.md`. Covers `include!`-only files too, so it fixes
  `launcher.rs` in the same stroke, and is immune to however modules come to be declared.
  Needs the edition passed explicitly, since there is no Cargo to supply it.
- **Drop the macro, write the two `#[cfg]` arms per module by hand.** ~22 lines in place of 8
  and restores plain `cargo fmt`, but the macro's comment already argues that case and chose
  otherwise; re-litigate it only if the first option proves awkward.
- Either way, reformat the 8 files that have drifted, as one whitespace-only commit kept
  separate from behavioral work.

`cargo clippy --workspace --all-targets` does **not** share the blind spot: it works from the
expanded crate graph. Verified by planting a lint-tripping function in
`repl/editor.rs` -- a macro-declared module rustfmt never opens -- and watching clippy report
it. So the two gates `CONTRIBUTING.md` lists side by side are not equivalent in coverage, and
only `fmt` needs fixing.

## Acceptance

- A deliberately misformatted `crates/outrig-cli/src/repl.rs` fails the documented gate.
- The set of files the gate checks equals the set of `.rs` files in the workspace, or the
  difference is written down.
- The 8 remaining misformatted files are clean.

## See also

- `plan/next/ci-configuration-coverage.md` -- the other "CI does not check what it appears
  to" entry.
