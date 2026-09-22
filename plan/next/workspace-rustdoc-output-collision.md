# `cargo doc --workspace` collides two targets onto one output path

`plan/todo/README.md` records this as having no entry yet. It does now.

## Symptom

`cargo doc --workspace --no-deps --locked` warns, on `trunk` at `c929ab9e`:

```
warning: output filename collision at target/doc/outrig/index.html
  = note: the bin target `outrig` in package `outrig-cli` has the same output filename as
    the lib target `outrig` in package `outrig`
```

`crates/outrig-cli` builds a binary named `outrig` -- which is the user-facing name and is not
going to change -- while `crates/outrig` is a library named `outrig`. Rustdoc derives the output
directory from the target name in both cases, so the two write to the same place and whichever
runs second wins. The rendered `target/doc/outrig/` is therefore whichever of the two happened
to finish last, which is not a property anyone should depend on.

## Why it is not urgent

Nothing consumes workspace-wide rustdoc output. The published docs site is mdbook
(`docs.yml` runs `mdbook build`), and 0002-52's CI gate is deliberately
`cargo rustdoc -p outrig --lib`, which names one target and so cannot collide. The
`public-api` job drives rustdoc through `cargo-public-api`, per crate.

So this costs nothing today. It becomes real the moment someone wants
`cargo doc --workspace` to produce a browsable tree -- publishing API docs beside the book, or
adding a docs job that renders both crates.

## Sketch

Options, cheapest first:

- **Leave it and never run `cargo doc --workspace`.** Document the restriction beside the
  rustdoc CI step so the next person does not "fix" it by widening that command, which is the
  likely accident. This is the status quo plus a sentence.
- **Render per package** -- `cargo doc -p outrig` and `cargo doc -p outrig-cli --lib` into
  separate `--target-dir`s, then stitch. Keeps both names.
- **Stop documenting the bin target.** A binary's rustdoc is close to worthless; `doc = false`
  on the `[[bin]]` in `crates/outrig-cli/Cargo.toml` removes the colliding target outright. The
  most direct fix, and worth checking whether it costs anything at all.

The last is probably right, but it wants confirming that nothing reads the bin's docs before it
is done.

## See also

- `.github/workflows/ci.yml` -- the `cargo rustdoc -p outrig --lib` step, whose `--lib` and
  `-p` are load-bearing for this reason.
- `plan/done/phase/0002-sidecars/tasks/0002-52-cut-0.2.0-rc.3.md` -- filed this while adding
  that step.
