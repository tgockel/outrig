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
