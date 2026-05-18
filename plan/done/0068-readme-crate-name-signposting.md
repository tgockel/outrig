# 0068 -- README and quickstart crate-name signposting

## Context

After the workspace split, the binary publishes as `outrig-cli` while
the library is `outrig`. A reader running `cargo install outrig` will
get the library and no binary -- a frustrating first encounter. The
README and `doc/quickstart.md` need to make the two-package layout
explicit.

## Goal

Make it obvious from a quick skim of `README.md` and `doc/quickstart.md`
which crate to install (or depend on) for which use case.

## Deliverables

- `README.md` install snippet updated to `cargo install outrig-cli`.
- A short "library vs CLI" callout near the top of `README.md` and
  `doc/quickstart.md`: one package for embedding (`outrig`), one for
  the command-line tool (`outrig-cli`), both publish from this repo.
- Note on crates.io reservation: confirm both `outrig` and
  `outrig-cli` are reserved, or capture the action item explicitly in
  the README so it isn't forgotten.

## Acceptance

- A reader who skims `README.md` knows which crate to depend on for
  which use case without reading source.
- `python3 scripts/audit-doc-style.py --width-only README.md
  doc/quickstart.md` reports `Width: OK`.

## Dependencies

- Soft on 0066 (so the README describes the final post-tidy-up shape
  of both crates) and on 0067 (so `--features local-llm` is the
  spelling that ships in the install snippet).

## Decisions

- crates.io returned 404 for both `outrig` and `outrig-cli` on
  2026-05-18, so the README records a release action instead of
  claiming either name is already reserved or published.
