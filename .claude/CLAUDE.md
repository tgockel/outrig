# OutRig

OutRig is a tool for running LLM agents against a repository, where every tool the agent invokes
runs inside of a `podman`-managed container described with a `Dockerfile`. Users are in control of
the environment the agent gets to play in, leaving the agents free to work quickly.

## Tree

- `doc/` -- design-first documentation. Start at `doc/README.md`.
- `plan/todo/` -- ordered implementation tasks (`NNNN-short-name.md`). The
  [`plan/todo/README.md`](../plan/todo/README.md) index lists every step with its phase.
  Each task has Goal, Deliverables, Acceptance, and Dependencies; a task depends only
  on lower-numbered predecessors.
- `plan/done/` -- completed tasks. Each file's `## Decisions` section is the
  authoritative record of design calls made during that task.
- `plan/next/` -- follow-up work discovered mid-execution (no leading `NNNN-`); folded
  into the numbered queue periodically.
- `src/` -- Rust source. Single-crate layout for v0; convert to a `crates/` workspace
  later if/when subsystems want their own published crates.
- `tests/` -- integration tests, including e2e tests gated behind `--features e2e`.
- `scripts/` -- repo-local tooling (doc-style audit, mdbook assets).
- `book.toml` -- mdbook config; output goes to `target/book/`.

## Conventions for `doc/`

- Maximum line width: **100 code points** (not bytes -- multi-byte chars count as one).
- **American English** spellings (behavior, optimize, recognize, color, etc.).
- Tables vertically aligned. If a row would exceed 100, convert to a definition-list
  bullet pattern instead.
- ASCII preferred where it has good equivalents (`--` for em dashes, `->` for one-way
  arrows); Unicode is welcome when clearer (footnote superscripts, table symbols, box
  drawing in diagrams).
- Subsystem docs carry `TODO: Incomplete` until their implementation lands; drop the line
  when the surface becomes real.

## Workflow

To advance one task: invoke `/next-task`. The skill picks the lowest-numbered file in
`plan/todo/`, branches, plans (drops into plan mode), executes, verifies (`cargo test`
/ `clippy` / `fmt` plus `/simplify`), confirms, and commits using conventional-commits
style. One task per branch.
