---
name: groom-plan
description: Maintain the invariants of plan/todo/ -- re-evaluate dependency order after edits to existing tasks, and pull entries from plan/next/ into the numbered queue. Use when the user types `/groom-plan`, says "groom the plan", or asks to refile / reorder / pull in follow-up work.
---

# groom-plan

Maintain the invariants of `plan/todo/`: every task depends only on smaller-numbered
predecessors, and `plan/next/` items get folded in with sensible sequence numbers.

## When to use

- **After editing existing tasks in `plan/todo/`** -- a change to a task's
  `## Dependencies` may now reference a higher-numbered task, violating the ordering rule.
- **When pulling work from `plan/next/` into `plan/todo/`** -- new tasks need a sequence
  number and may slot between existing tasks, causing renumbering.

## Preflight

Verify the working tree is clean:

```bash
git status --porcelain
```

If non-empty, **abort** and tell the user to commit or stash first. Grooming should be a
clean, isolated operation so the resulting commit is easy to read.

## 1. Survey

Validate the current `plan/todo/` ordering:

```bash
python3 - <<'PY'
import re, os, glob
violations = []
for path in sorted(glob.glob('plan/todo/[0-9]*.md')):
    fname = os.path.basename(path)
    seq = int(fname[:4])
    text = open(path).read()
    m = re.search(r'## Dependencies\s*\n(.*?)(?=\n##|\Z)', text, re.DOTALL)
    if not m:
        continue
    deps = m.group(1)
    for dm in re.finditer(r'(?:^|\s)(\d{4})\b', deps):
        if int(dm.group(1)) >= seq:
            violations.append(f"{fname}: depends on {dm.group(1)} not < {seq:04d}")
print("\n".join(violations) if violations else "OK")
PY
```

List anything in `plan/next/`:

```bash
ls plan/next/*.md 2>/dev/null || echo "(empty)"
```

Read the content of each violating task and each `plan/next/` candidate to understand
their dependency shape.

## 2. Plan the changes

Together with the user, decide:

- Which violations need to be fixed and how (renumber the offending task, or renumber its
  dependencies).
- Which `plan/next/` entries to pull in, and where they should slot.

For each move or pull-in, identify the new sequence number that satisfies:

- greater than every dependency the task has,
- smaller than every task that already depends on it.

If no free number exists between those bounds, renumber other tasks to make room. Aim to
minimize the number of files touched.

For non-trivial reshuffles, drop into plan mode and write the proposed renumbering plan
to the plan file before executing.

## 3. Apply

For each renumber or pull-in:

- Rename the file via `git mv plan/<old-path> plan/<new-path>`. For `plan/next/` entries
  this also adds the `NNNN-` prefix to the filename.
- Update the file's title heading (the `# NNNN - short-name` line at the top) to the new
  number.
- Update the `## Dependencies` section of every other `plan/todo/` file that references
  the old number, replacing it with the new number.
- For pulled-in `plan/next/` entries, ensure the file has the four standard sections
  (Goal, Deliverables, Acceptance, Dependencies). Add any missing ones in collaboration
  with the user.

## 4. Update the index

Edit `plan/todo/README.md` so its index table reflects the new state: new tasks added,
moved tasks at their new positions, order matches the on-disk sequence.

## 5. Verify

Re-run the validation script from step 1; it must report `OK`. Spot-check that every
file's title heading matches its filename.

If `plan/todo/README.md` was edited, verify it still satisfies the line-width rule:

```bash
python3 scripts/audit-doc-style.py --width-only plan/todo/README.md
```

The script prints `Width: OK` when clean and exits non-zero on any line over 120
code points.

## 6. Confirm

Summarize the changes for the user before committing:

- which tasks were renumbered (old number -> new number),
- which `plan/next/` entries were pulled in (and to what number),
- which other files had their dependency references updated,
- the index changes.

## 7. Commit

```bash
git add -A
git commit -m "$(cat <<EOF
plan: <short summary>

<Detailed description>
EOF
)"
```

Conventional-commits style; `plan:` since this is plan-tree maintenance, not code.

**Keep line width under 72 characters** in the commit message. This is a soft rule
you can exceed when necessary for readability.

**Do not include a co-authored-by trailer.**

Do not push -- the user merges or pushes when ready.

## Notes

- This skill does NOT modify task content beyond title headings and dependency
  references. Goal / Deliverables / Acceptance are owned by the task author and are
  preserved verbatim.
- This skill does NOT touch `plan/done/`. Done tasks have stable historical numbers; if
  a done task is referenced by a still-todo task and the todo task is renumbered, only
  the todo task's title changes.
- A `plan/next/` entry that depends on another `plan/next/` entry must be pulled in
  alongside its dependency, with consistent ordering.
- This is a maintenance operation, not a planning operation -- the *content* of tasks
  doesn't change here. If new requirements emerge, edit the task file (which may then
  call for another `/groom-plan` run).
