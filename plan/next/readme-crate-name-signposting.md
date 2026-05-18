# README signposting for `outrig` vs `outrig-cli`

## Goal

After the workspace split, the published crate that produces the `outrig`
binary is named `outrig-cli`, while `outrig` is the library crate. New
users running `cargo install outrig` (instead of `outrig-cli`) will get
nothing useful. Add explicit signposting.

## Tasks

- Update `README.md` install snippet to use `cargo install outrig-cli`.
- Add a short "library vs CLI" callout near the top of the README and
  `doc/quickstart.md`: one package for embedding (`outrig`), one for the
  command-line tool (`outrig-cli`), both publish from this repo.
- If we publish to crates.io: confirm `outrig` (the library) and
  `outrig-cli` (the binary) are both reserved.

## Acceptance

A reader who skims the README knows which package to depend on for which
use case without having to read code.
