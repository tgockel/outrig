# 0082 -- Fix pre-existing doc-style audit violations

## Goal

Make `python3 scripts/audit-doc-style.py` pass on trunk. It currently fails (exit 1) with
two width violations that predate task 0079:

- `doc/reference/cli.md:263` -- 109 code points (a table row in the `--image` flag table)
- `doc/usage/run.md:310` -- 155 code points (an error-message sample line)

Both exceed the 100-code-point width rule.

## Deliverables

- Rewrap `doc/reference/cli.md:263` per the "convert to a definition-list bullet pattern"
  convention for over-width table rows.
- Break the long error-message sample in `doc/usage/run.md:310` across lines.
- CI runs the same script (`.github/workflows/ci.yml`), so either CI is currently red on
  doc-style or the workflow doesn't gate on it; determine which and fix the gating if it
  is broken.

## Acceptance

- `python3 scripts/audit-doc-style.py` exits 0 on trunk.
- The CI workflow demonstrably gates on the doc-style script.

## Dependencies

None.

## Decisions

- By the time this task was picked up, both width violations had already been fixed by
  intervening commits (most recently `8435f932`, which reworked the `cli.md` flag table
  into its current in-width form). `python3 scripts/audit-doc-style.py` exits 0 on trunk,
  so no doc edits were made here.
- The CI question resolved to "the workflow doesn't gate on it": `ci.yml` never invoked
  the audit script, despite CLAUDE.md claiming CI runs it. Fixed by adding a `doc-style`
  job (checkout + `python3 scripts/audit-doc-style.py`). The script is stdlib-only, so
  the job needs no pip install; its non-zero exit on violations is what gates.
