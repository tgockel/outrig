---
name: next-task
description: Pick up the lowest-numbered task from plan/todo/, design and execute it on its own branch, verify with cargo and /simplify, and commit using conventional-commits style. One task per branch. Use when the user types `/next-task`, says "next task", or asks to advance the implementation queue.
---

# next-task

Walk one task end-to-end: pick the next file from `plan/todo/`, plan, execute, verify, commit.

## 1. Preflight

Find the lowest-numbered file in `plan/todo/` (skip `README.md`):

```bash
ls plan/todo/[0-9]*.md | sort | head -1
```

Read the task file. Extract its `## Dependencies` list.

Verify every dependency is in `plan/done/` (not in `plan/todo/`). If any dep is unmet,
**abort** and tell the user which task needs to be completed first.

Verify the working tree is clean:

```bash
git status --porcelain
```

If the output is non-empty, **abort** and tell the user to commit or stash first.

## 2. Branch

Create a branch named for the task's short-name (e.g. `0001-cargo-skeleton.md` becomes
branch `0001-cargo-skeleton`). One task per branch:

```bash
git checkout -b "<short-name>"
```

## 3. Context

- Read the task's plan markdown.
- Read every `doc/` and `src/` file referenced (or implied) by the task.
- Drop into plan mode for the standard Explore-agent / Plan-agent flow.

## 4. Clarify

Use `AskUserQuestion` for any unclear intent. If the user's clarification changes the
design, **edit the task's plan file in place** to record the revised plan before
executing.

Exit plan mode when ready.

## 5. Execute

Implement the deliverables.

As you make non-obvious design calls during execution, append them to a `## Decisions`
section in the task's plan file (still in `plan/todo/` at this point). The section
becomes part of the historical record once the task moves to `plan/done/`.

If you discover follow-up work outside this task's scope, file it as
`plan/next/<short-name>.md` (no leading `NNNN-`). The user folds these into the numbered
queue periodically.

## 6. Verify

Run the `/simplify` skill on the changes (review for reuse, quality, efficiency):

Then run the standard cargo checks:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt -- --check
```

All three must pass. If any fail, fix and re-run -- do not commit broken code.

Walk the task's `## Acceptance` criteria explicitly. For each item in that list, identify
the specific test (or assertion) that exercises it. If any criterion has no test
covering it, write one. `cargo test` passing is necessary but not sufficient -- a
criterion could be missing a test entirely.

If anything in `doc/` changed, run the doc-style audit:

```bash
python3 scripts/audit-doc-style.py
```

The script prints `Width: OK`, `Links: OK`, `Spelling: OK` when clean, and exits
non-zero if any check finds violations. CI runs the same script (see
`.github/workflows/ci.yml`).

Update / expand `doc/` to match the implemented surface. Specifically: drop the
`> TODO: Incomplete` blockquote from any `doc/{concepts,usage,reference}/` page whose
behavior is now fully real. Each task's `## Acceptance` section lists the markers it's
expected to drop.

## 7. Confirm

Summarize what changed:

- which deliverables landed,
- which acceptance criteria they map to,
- which docs were updated,
- which decisions were recorded.

Verify with the user that the result aligns with expectations before committing. (If
auto-mode is active and the user has opted into autonomous execution, skip the explicit
confirmation.)

## 8. Land

Move the task file from `plan/todo/` to `plan/done/`:

```bash
git mv plan/todo/<NNNN-short-name>.md plan/done/<NNNN-short-name>.md
```

Update `plan/todo/README.md` -- remove the task's row from the per-step index table.

Stage all changes and commit using conventional-commits style:

| Prefix     | When                                                  |
|------------|-------------------------------------------------------|
| `feat:`    | New feature or capability (most implementation tasks) |
| `fix:`     | Bug fix                                               |
| `chore:`   | Build, tooling, dependencies                          |
| `docs:`    | Documentation-only change                             |
| `test:`    | Test-only change                                      |
| `refactor:`| Restructuring without behavior change                 |
| `plan:`    | Plan-only changes (e.g. clarifying the plan file)     |
| `ai:`      | AI-only changes (e.g. prompt tweaks)                  |

```bash
git add -A
git commit -m "$(cat <<EOF
<type>: <short summary>

<Detailed description>
EOF
)"
```

Use the heredoc form above for multi-paragraph messages. Put real line breaks in
the message body; do not try to fake wrapping with escaped `\n` inside `-m`
arguments, and do not build the body from several long unwrapped `-m` arguments.

Before running `git commit`, inspect recent local history:

```bash
git log --format=fuller -5
```

Match the repo's house style: a conventional subject plus wrapped body
paragraphs with concrete behavior, design, compatibility, and verification
notes. Avoid vague filler and one-line marketing summaries.

**Keep line width under 72 characters** in the commit message body. This is a
soft rule you can exceed when necessary for readability, but count the actual
message lines before committing if there is any doubt.

**Do not include the task number in the commit message.** The connection between commit
and task is via the file in `plan/done/`. `git log` plus `plan/done/` searches are
sufficient for archaeology.

**Do not include a co-authored-by trailer.**

Do not push -- the user merges / pushes when ready.

## After

The task is complete. The branch holds one commit; `plan/todo/` has shrunk by one,
`plan/done/` has grown by one. The user can run `/next-task` again to pick up the next
file in the queue (after merging this branch back to the trunk).
