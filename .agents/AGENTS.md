# OutRig

OutRig is a tool for running LLM agents against a repository, where every tool the agent invokes
runs inside of a `podman`-managed container described with a `Dockerfile`. Users are in control of
the environment the agent gets to play in, leaving the agents free to work quickly.

## Tree

- `doc/` -- design-first documentation. Start at `doc/README.md`.
- `plan/todo/` -- ordered implementation tasks (`PPPP-NN-short-name.md`, where `PPPP`
  is the 4-digit phase number and `NN` the 2-digit sequence within that phase). The
  [`plan/todo/README.md`](../plan/todo/README.md) index lists every step with its phase.
  Each task has Goal, Deliverables, Acceptance, and Dependencies; a task depends only
  on lower-numbered predecessors in the same ordering.
- `plan/done/` -- completed tasks, bucketed by phase as
  `plan/done/phase/<PPPP>-<name>/tasks/PPPP-NN-short-name.md`. A task lands there as soon
  as it is finished, so the folder appears while its phase is still open. Each file's
  `## Decisions` section is the authoritative record of design calls made during that task.
- `plan/phase/` -- open phase definitions as `<PPPP>-<name>/README.md` (Goal, user-visible
  deliverables, exit criteria, linked subsystems, tasks, out of scope). When a phase closes
  its README moves into `plan/done/phase/<PPPP>-<name>/`, beside the `tasks/` folder already
  there. Multiple phases may be open at once; a task belongs to one via its `PPPP-` prefix.
- `plan/next/` -- buffer for follow-up work discovered mid-execution (no leading
  `PPPP-NN-`). Drop entries here so the current task stays focused; the queue is folded
  into `plan/todo/` periodically.
- `crates/` -- the Rust workspace: `outrig`, the library, and `outrig-cli`, the binary.
  Each carries its own `src/`, `tests/`, `README.md`, `CHANGELOG.md`, and
  `public-api.txt` snapshot.
- `crates/*/tests/` -- integration tests. The e2e ones need a real podman and are gated
  behind each crate's `e2e` feature (`--features outrig/e2e,outrig-cli/e2e`); CI compiles
  them without running them.
- `scripts/` -- repo-local tooling (doc-style audit, public-API snapshot gate, mdbook
  assets).
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

When you notice work outside the current task's scope -- a latent bug, a v1
follow-up, a refactor that wants doing later -- one good option is to drop a short
spec into `plan/next/<short-name>.md` and keep going. The directory is a "for
later" pool, useful when the thought is worth capturing but doesn't need to derail
the current task. Entries get folded into the numbered queue when the user runs
`/groom-plan`.
